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

/// Deriva a chave direto no buffer do chamador: devolver o array por valor
/// deixaria copias da chave na pilha. O chamador usa `Zeroizing`, que zera a
/// chave no drop em qualquer caminho, inclusive nos retornos de erro.
fn derive_key(password: &str, salt: &[u8], key: &mut [u8; 32]) -> anyhow::Result<()> {
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, key)
        .map_err(|e| anyhow::anyhow!("falha na derivacao da chave: {e}"))
}

/// Criptografa o cofre com a senha mestra, retornando os bytes do arquivo.
pub fn encrypt_vault(vault: &Vault, password: &str) -> anyhow::Result<Vec<u8>> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let mut key = Zeroizing::new([0u8; 32]);
    derive_key(password, &salt, &mut key)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_slice()));
    let plaintext = serde_json::to_vec(vault)?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_ref())
        .map_err(|e| anyhow::anyhow!("falha ao criptografar: {e}"))?;

    let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Descriptografa os bytes de um arquivo de cofre com a senha mestra.
pub fn decrypt_vault(data: &[u8], password: &str) -> anyhow::Result<Vault> {
    if data.len() < HEADER_LEN || &data[..8] != MAGIC {
        anyhow::bail!("arquivo de cofre invalido");
    }
    let salt = &data[8..8 + SALT_LEN];
    let nonce_bytes = &data[8 + SALT_LEN..HEADER_LEN];
    let ciphertext = &data[HEADER_LEN..];

    let mut key = Zeroizing::new([0u8; 32]);
    derive_key(password, salt, &mut key)?;
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_slice()));
    let plaintext = cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
        .map_err(|_| anyhow::anyhow!("senha mestra incorreta ou arquivo corrompido"))?;

    let vault: Vault = serde_json::from_slice(&plaintext)?;
    Ok(vault)
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

        let bytes = encrypt_vault(&vault, "master").unwrap();
        let back = decrypt_vault(&bytes, "master").unwrap();
        assert_eq!(back.hosts.len(), 1);
        assert_eq!(back.hosts[0].host, "example.com");

        assert!(decrypt_vault(&bytes, "wrong").is_err());
    }

    #[test]
    fn old_vault_without_host_key_opens() {
        // JSON de um cofre gravado antes do campo existir.
        let json = r#"{"hosts":[{"id":"6f1c2a0e-8a4b-4c1d-9e2f-3a4b5c6d7e8f","name":"a",
            "host":"h","port":22,"username":"u","auth":{"Password":{"password":"p"}}}]}"#;
        let vault: Vault = serde_json::from_str(json).unwrap();
        assert_eq!(vault.hosts.len(), 1);
        assert_eq!(vault.hosts[0].host_key, None);

        // A chave aceita sobrevive a gravacao cifrada.
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDx116/S6vbyAU3ZR1ebTYjMs187ZiPcltXd5Dg8Oapm";
        let mut vault = vault;
        vault.hosts[0].host_key = Some(key.to_string());
        let bytes = encrypt_vault(&vault, "master").unwrap();
        let back = decrypt_vault(&bytes, "master").unwrap();
        assert_eq!(back.hosts[0].host_key.as_deref(), Some(key));
    }
}
