//! Cofre criptografado portatil.
//!
//! Formato do arquivo (`*.sagu`):
//! ```text
//! [ 8 bytes ] MAGIC  = "SAGUVLT1"
//! [16 bytes ] salt   (Argon2id)
//! [12 bytes ] nonce  (AES-256-GCM)
//! [ N bytes ] ciphertext = AES-256-GCM( JSON(Vault) )
//! ```
//! A chave de 32 bytes e derivada da senha mestra com Argon2id. O arquivo e
//! autossuficiente: pode ser copiado para outra maquina e aberto com a mesma
//! senha mestra.
//!
//! O sal nasce com o cofre e se mantem nas gravacoes seguintes (so o nonce e
//! novo a cada uma): o app guarda a chave derivada em vez da senha (ver
//! [`VaultKey`]), e ela continua valendo para o arquivo regravado.

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Key, Nonce,
};
use argon2::Argon2;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"SAGUVLT1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 8 + SALT_LEN + NONCE_LEN;

/// Metodo de autenticacao de um host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AuthMethod {
    /// Autenticacao por senha.
    Password { password: String },
    /// Autenticacao por chave privada (PEM/OpenSSH), com passphrase opcional.
    Key {
        private_key: String,
        passphrase: Option<String>,
    },
}

impl Default for AuthMethod {
    fn default() -> Self {
        AuthMethod::Password {
            password: String::new(),
        }
    }
}

/// Um host SSH cadastrado.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    /// Chave publica do servidor ja aceita pelo usuario, formato OpenSSH
    /// "algoritmo base64" (sem comentario). `None` = ainda nao confiada: a
    /// proxima conexao pergunta. Cofres antigos nao tem o campo (serde default).
    #[serde(default)]
    pub host_key: Option<String>,
    /// Sistema detectado no servidor (ver osinfo), para o icone e a dica do
    /// cartao. `None` = nao identificado. Apagado quando endereco ou porta mudam no
    /// editor. Cofres antigos nao tem o campo (serde default).
    #[serde(default)]
    pub os: Option<crate::osinfo::OsInfo>,
    /// Detectar o sistema do servidor ao conectar (ver osinfo). Desligavel no
    /// editor: num servidor que forca um comando (ForceCommand, command= no
    /// authorized_keys), e ele que rodaria no lugar da sonda. Cofres antigos
    /// nao tem o campo: ligado.
    #[serde(default = "detect_os_default")]
    pub detect_os: bool,
}

fn detect_os_default() -> bool {
    true
}

impl Host {
    pub fn new() -> Self {
        Host {
            id: Uuid::new_v4(),
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth: AuthMethod::default(),
            host_key: None,
            os: None,
            detect_os: true,
        }
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

/// Conteudo do cofre.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Vault {
    pub hosts: Vec<Host>,
}

/// Chave do cofre aberto: o sal do arquivo e a chave de 32 bytes derivada dele
/// com a senha mestra. O app a mantem na memoria no lugar da senha (salvar nao
/// roda o Argon2 de novo) e o "abrir sem senha neste computador" a guarda
/// protegida pelo Windows (ver remember). A chave e zerada no drop; sem Clone,
/// para nao espalhar copias dela.
pub struct VaultKey {
    salt: [u8; SALT_LEN],
    key: Zeroizing<[u8; 32]>,
}

impl VaultKey {
    /// Chave de um cofre novo: sal aleatorio e Argon2id da senha.
    pub fn new(password: &str) -> anyhow::Result<Self> {
        let mut salt = [0u8; SALT_LEN];
        rand::thread_rng().fill_bytes(&mut salt);
        Self::derive(password, salt)
    }

    /// Deriva a chave direto no buffer zerado no drop: devolver o array por
    /// valor deixaria copias da chave na pilha.
    fn derive(password: &str, salt: [u8; SALT_LEN]) -> anyhow::Result<Self> {
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::default()
            .hash_password_into(password.as_bytes(), &salt, key.as_mut_slice())
            .map_err(|e| anyhow::anyhow!("falha na derivação da chave: {e}"))?;
        Ok(VaultKey { salt, key })
    }

    /// Chave guardada antes (sal do arquivo + os 32 bytes); `None` se os
    /// tamanhos nao conferem.
    pub fn from_parts(salt: &[u8], key: &[u8]) -> Option<Self> {
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(key.get(..32).filter(|_| key.len() == 32)?);
        Some(VaultKey {
            salt: salt.try_into().ok()?,
            key: k,
        })
    }

    /// Sal do cofre (publico: vai no cabecalho do arquivo).
    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    /// Os 32 bytes da chave (segredo).
    pub fn secret(&self) -> &[u8] {
        self.key.as_slice()
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(self.key.as_slice()))
    }
}

/// Sal do cabecalho de um arquivo de cofre; `None` se nao for um cofre.
pub fn file_salt(data: &[u8]) -> Option<&[u8]> {
    if data.len() < HEADER_LEN || &data[..8] != MAGIC {
        return None;
    }
    Some(&data[8..8 + SALT_LEN])
}

/// Criptografa o cofre com a chave (sal dela, nonce novo), retornando os
/// bytes do arquivo.
pub fn encrypt_vault(vault: &Vault, key: &VaultKey) -> anyhow::Result<Vec<u8>> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let plaintext = Zeroizing::new(serde_json::to_vec(vault)?);
    let ciphertext = key
        .cipher()
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_ref())
        .map_err(|e| anyhow::anyhow!("falha ao criptografar: {e}"))?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&key.salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Descriptografa os bytes de um arquivo de cofre com a senha mestra. Devolve
/// tambem a chave, para as proximas gravacoes.
pub fn decrypt_vault(data: &[u8], password: &str) -> anyhow::Result<(Vault, VaultKey)> {
    let salt = file_salt(data).ok_or_else(|| anyhow::anyhow!("arquivo de cofre inválido"))?;
    let key = VaultKey::derive(password, salt.try_into()?)?;
    let vault = decrypt(data, &key)
        .map_err(|_| anyhow::anyhow!("senha mestra incorreta ou arquivo corrompido"))?;
    Ok((vault, key))
}

/// Descriptografa os bytes de um arquivo de cofre com uma chave guardada.
/// Falha se o arquivo foi regravado com outro sal (cofre recriado, ou salvo
/// por uma versao anterior do app, que troca o sal a cada gravacao).
pub fn decrypt_with_key(data: &[u8], key: &VaultKey) -> anyhow::Result<Vault> {
    match file_salt(data) {
        None => anyhow::bail!("arquivo de cofre inválido"),
        Some(salt) if salt != key.salt => anyhow::bail!("a chave guardada não é deste cofre"),
        Some(_) => decrypt(data, key),
    }
}

fn decrypt(data: &[u8], key: &VaultKey) -> anyhow::Result<Vault> {
    let nonce_bytes = &data[8 + SALT_LEN..HEADER_LEN];
    let plaintext = Zeroizing::new(
        key.cipher()
            .decrypt(Nonce::from_slice(nonce_bytes), &data[HEADER_LEN..])
            .map_err(|_| anyhow::anyhow!("chave incorreta ou arquivo corrompido"))?,
    );
    Ok(serde_json::from_slice(&plaintext)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut vault = Vault::default();
        let mut h = Host::new();
        h.name = "server".into();
        h.host = "example.com".into();
        h.username = "root".into();
        h.auth = AuthMethod::Password {
            password: "secret".into(),
        };
        vault.hosts.push(h);

        let key = VaultKey::new("master").unwrap();
        let bytes = encrypt_vault(&vault, &key).unwrap();
        let (back, back_key) = decrypt_vault(&bytes, "master").unwrap();
        assert_eq!(back.hosts.len(), 1);
        assert_eq!(back.hosts[0].host, "example.com");
        // A senha devolve a mesma chave (mesmo sal), que serve para regravar.
        assert_eq!(back_key.salt(), key.salt());
        assert_eq!(back_key.secret(), key.secret());

        assert!(decrypt_vault(&bytes, "wrong").is_err());
    }

    /// Gravacoes com a chave mantem o sal (nonce novo a cada uma) e abrem sem
    /// a senha; a chave de outro cofre, ou adulterada, nao abre.
    #[test]
    fn key_keeps_salt_and_opens_without_password() {
        let key = VaultKey::new("master").unwrap();
        let mut vault = Vault::default();
        let a = encrypt_vault(&vault, &key).unwrap();
        vault.hosts.push(Host::new());
        let b = encrypt_vault(&vault, &key).unwrap();
        assert_eq!(file_salt(&a), Some(key.salt()));
        assert_eq!(file_salt(&b), Some(key.salt()));
        assert_ne!(a[8 + SALT_LEN..HEADER_LEN], b[8 + SALT_LEN..HEADER_LEN], "nonce novo");

        // Guardada em partes (como no "abrir sem senha") e remontada.
        let stored = VaultKey::from_parts(key.salt(), key.secret()).unwrap();
        assert_eq!(decrypt_with_key(&a, &stored).unwrap().hosts.len(), 0);
        assert_eq!(decrypt_with_key(&b, &stored).unwrap().hosts.len(), 1);
        assert_eq!(decrypt_vault(&b, "master").unwrap().0.hosts.len(), 1);

        // Mesma senha, outro cofre: outro sal, a chave nao serve.
        let other = VaultKey::new("master").unwrap();
        assert_ne!(other.salt(), key.salt());
        let e = decrypt_with_key(&b, &other).unwrap_err().to_string();
        assert!(e.contains("não é deste cofre"), "{e}");
        // Sal certo, chave errada.
        let mut bad = key.secret().to_vec();
        bad[0] ^= 1;
        let bad = VaultKey::from_parts(key.salt(), &bad).unwrap();
        assert!(decrypt_with_key(&b, &bad).is_err());

        // Tamanhos errados e arquivo que nao e cofre.
        assert!(VaultKey::from_parts(key.salt(), &[0u8; 31]).is_none());
        assert!(VaultKey::from_parts(&[0u8; 15], key.secret()).is_none());
        assert_eq!(file_salt(b"SAGUVLT1curto"), None);
        assert_eq!(file_salt(&[0u8; 64]), None);
        assert!(decrypt_with_key(&[0u8; 64], &key).is_err());
    }

    #[test]
    fn old_vault_without_host_key_opens() {
        // JSON de um cofre gravado antes dos campos existirem.
        let json = r#"{"hosts":[{"id":"6f1c2a0e-8a4b-4c1d-9e2f-3a4b5c6d7e8f","name":"a",
            "host":"h","port":22,"username":"u","auth":{"Password":{"password":"p"}}}]}"#;
        let vault: Vault = serde_json::from_str(json).unwrap();
        assert_eq!(vault.hosts.len(), 1);
        assert_eq!(vault.hosts[0].host_key, None);
        assert_eq!(vault.hosts[0].os, None);
        assert!(vault.hosts[0].detect_os, "cofre antigo: deteccao ligada");

        // A chave aceita e o SO detectado sobrevivem a gravacao cifrada.
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDx116/S6vbyAU3ZR1ebTYjMs187ZiPcltXd5Dg8Oapm";
        let os = crate::osinfo::OsInfo {
            id: "almalinux".into(),
            name: "AlmaLinux".into(),
            version: Some("8.10".into()),
        };
        let mut vault = vault;
        vault.hosts[0].host_key = Some(key.to_string());
        vault.hosts[0].os = Some(os.clone());
        let master = VaultKey::new("master").unwrap();
        let bytes = encrypt_vault(&vault, &master).unwrap();
        let back = decrypt_vault(&bytes, "master").unwrap().0;
        assert_eq!(back.hosts[0].host_key.as_deref(), Some(key));
        assert_eq!(back.hosts[0].os.as_ref(), Some(&os));
        assert!(back.hosts[0].detect_os);

        // Deteccao desligada no editor sobrevive a gravacao.
        let mut vault = back;
        vault.hosts[0].detect_os = false;
        let bytes = encrypt_vault(&vault, &master).unwrap();
        assert!(!decrypt_vault(&bytes, "master").unwrap().0.hosts[0].detect_os);

        // Campo desconhecido (versao futura) nao impede de abrir; SO sem a
        // versao (distribuicao continua) tambem abre.
        let json = r#"{"hosts":[{"id":"6f1c2a0e-8a4b-4c1d-9e2f-3a4b5c6d7e8f","name":"a",
            "host":"h","port":22,"username":"u","auth":{"Password":{"password":"p"}},
            "os":{"id":"arch","name":"Arch Linux"},"campo_novo":[1,2]}]}"#;
        let vault: Vault = serde_json::from_str(json).unwrap();
        let os = vault.hosts[0].os.as_ref().unwrap();
        assert_eq!((os.id.as_str(), os.version.as_deref()), ("arch", None));
    }
}
