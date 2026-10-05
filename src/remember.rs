//! "Abrir sem senha neste computador": a chave do cofre aberto (nunca a senha
//! mestra) fica num arquivo ao lado do app.ron, protegida pela conta do
//! Windows com a DPAPI. So o mesmo usuario do Windows, neste computador,
//! consegue desprotege-la; o .sagu copiado para outro lugar continua pedindo
//! a senha.
//!
//! Cada cofre e achado pelo sal do cabecalho (publico), que tambem entra na
//! entropia da DPAPI: a chave guardada so serve para aquele cofre. Se o cofre
//! for regravado com outro sal (por uma versao anterior do app), a entrada
//! deixa de valer e o app volta a pedir a senha.
//!
//! Bloquear o cofre suspende a entrada: a proxima abertura do app pede a
//! senha (sem isso, fechar e abrir o app desfaria o bloqueio), e digita-la
//! religa a abertura automatica.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::vault::VaultKey;

/// Nome do arquivo na pasta de dados do app.
const FILE_NAME: &str = "remembered.json";

/// Prefixo da entropia da DPAPI (seguido do sal do cofre).
const ENTROPY: &[u8] = b"SaguTerm: abrir sem senha\0";

/// Situacao do cofre neste computador.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Pede a senha (opcao desligada).
    Off,
    /// Abre sozinho ao iniciar o app.
    On,
    /// Ligado, mas bloqueado pelo usuario: a proxima abertura pede a senha.
    Suspended,
}

#[derive(Default, Serialize, Deserialize)]
struct Store {
    vaults: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    /// Sal do cofre (hex), que identifica o arquivo.
    salt: String,
    /// Chave do cofre protegida pela DPAPI (hex).
    key: String,
    #[serde(default)]
    suspended: bool,
}

/// Arquivo das chaves guardadas, na pasta do app.ron
/// (`%APPDATA%\SaguTerm\data`).
pub fn default_file() -> Option<PathBuf> {
    eframe::storage_dir(crate::APP_NAME).map(|d| d.join(FILE_NAME))
}

pub fn state(file: &Path, salt: &[u8]) -> State {
    match load(file).vaults.iter().find(|e| e.salt == hex(salt)) {
        None => State::Off,
        Some(e) if e.suspended => State::Suspended,
        Some(_) => State::On,
    }
}

/// Guarda (ou renova) a chave do cofre e liga a abertura automatica.
pub fn enable(file: &Path, key: &VaultKey) -> anyhow::Result<()> {
    let blob = dpapi::protect(key.secret(), &entropy(key.salt()))?;
    let mut store = load(file);
    let salt = hex(key.salt());
    store.vaults.retain(|e| e.salt != salt);
    store.vaults.push(Entry {
        salt,
        key: hex(&blob),
        suspended: false,
    });
    save(file, &store)
}

/// Apaga a chave guardada do cofre.
pub fn disable(file: &Path, salt: &[u8]) -> anyhow::Result<()> {
    let mut store = load(file);
    let salt = hex(salt);
    let before = store.vaults.len();
    store.vaults.retain(|e| e.salt != salt);
    if store.vaults.len() == before {
        return Ok(());
    }
    save(file, &store)
}

/// Bloqueio: mantem a chave, mas a proxima abertura do app pede a senha.
pub fn suspend(file: &Path, salt: &[u8]) -> anyhow::Result<()> {
    let mut store = load(file);
    let salt = hex(salt);
    match store.vaults.iter_mut().find(|e| e.salt == salt) {
        Some(e) if !e.suspended => e.suspended = true,
        _ => return Ok(()),
    }
    save(file, &store)
}

/// Chave guardada do cofre com este sal. `Ok(None)`: nao guardada ou
/// suspensa. Erro: guardada, mas o Windows nao a devolveu (outra conta,
/// senha do Windows redefinida, arquivo adulterado).
pub fn key_for(file: &Path, salt: &[u8]) -> anyhow::Result<Option<VaultKey>> {
    let store = load(file);
    let Some(e) = store.vaults.iter().find(|e| e.salt == hex(salt)) else {
        return Ok(None);
    };
    if e.suspended {
        return Ok(None);
    }
    let blob = unhex(&e.key).ok_or_else(|| anyhow::anyhow!("chave guardada ilegível"))?;
    let secret = dpapi::unprotect(&blob, &entropy(salt))?;
    VaultKey::from_parts(salt, &secret)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("chave guardada inválida"))
}

fn entropy(salt: &[u8]) -> Vec<u8> {
    [ENTROPY, salt].concat()
}

/// Arquivo ausente ou ilegivel conta como vazio.
fn load(file: &Path) -> Store {
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Escrita atomica, como a do cofre: temporario ao lado e rename por cima.
fn save(file: &Path, store: &Store) -> anyhow::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(store)?)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, file)?;
    Ok(())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(windows)]
mod dpapi {
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::{Zeroize, Zeroizing};

    fn blob(b: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: b.len() as u32,
            pbData: b.as_ptr() as *mut u8,
        }
    }

    /// Copia a saida da DPAPI e a devolve ao sistema, zerada antes (na
    /// desprotecao e a chave em claro).
    unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Zeroizing<Vec<u8>> {
        if out.pbData.is_null() {
            return Zeroizing::new(Vec::new());
        }
        let s = unsafe { std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize) };
        let v = Zeroizing::new(s.to_vec());
        s.zeroize();
        unsafe { LocalFree(Some(HLOCAL(out.pbData as _))) };
        v
    }

    pub fn protect(data: &[u8], entropy: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &blob(data),
                w!("SaguTerm"),
                Some(&blob(entropy)),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| anyhow::anyhow!("o Windows não protegeu a chave: {}", e.message()))?;
            Ok(take(out).to_vec())
        }
    }

    pub fn unprotect(data: &[u8], entropy: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(
                &blob(data),
                None,
                Some(&blob(entropy)),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .map_err(|e| anyhow::anyhow!("o Windows não devolveu a chave: {}", e.message()))?;
            Ok(take(out))
        }
    }
}

#[cfg(not(windows))]
mod dpapi {
    use zeroize::Zeroizing;

    pub fn protect(_: &[u8], _: &[u8]) -> anyhow::Result<Vec<u8>> {
        anyhow::bail!("disponível só no Windows")
    }

    pub fn unprotect(_: &[u8], _: &[u8]) -> anyhow::Result<Zeroizing<Vec<u8>>> {
        anyhow::bail!("disponível só no Windows")
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sagu-remember-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("data").join(FILE_NAME)
    }

    #[test]
    fn enable_suspend_disable() {
        let file = temp_file("ciclo");
        let a = VaultKey::new("a").unwrap();
        let b = VaultKey::new("b").unwrap();
        assert_eq!(state(&file, a.salt()), State::Off);
        assert!(key_for(&file, a.salt()).unwrap().is_none());

        // Liga (cria a pasta); a chave volta igual.
        enable(&file, &a).unwrap();
        enable(&file, &b).unwrap();
        assert_eq!(state(&file, a.salt()), State::On);
        let back = key_for(&file, a.salt()).unwrap().unwrap();
        assert_eq!((back.salt(), back.secret()), (a.salt(), a.secret()));

        // No arquivo: so o sal e o blob da DPAPI, nunca a chave em claro.
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains(&hex(a.salt())));
        assert!(!text.contains(&hex(a.secret())));

        // Bloquear suspende so este cofre; ligar de novo reativa.
        suspend(&file, a.salt()).unwrap();
        assert_eq!(state(&file, a.salt()), State::Suspended);
        assert!(key_for(&file, a.salt()).unwrap().is_none());
        assert_eq!(state(&file, b.salt()), State::On);
        enable(&file, &a).unwrap();
        assert_eq!(state(&file, a.salt()), State::On);
        assert_eq!(load(&file).vaults.len(), 2, "renovar nao duplica");

        // Desligar apaga so este.
        disable(&file, a.salt()).unwrap();
        assert_eq!(state(&file, a.salt()), State::Off);
        assert_eq!(state(&file, b.salt()), State::On);
        disable(&file, a.salt()).unwrap();

        let _ = std::fs::remove_dir_all(file.parent().unwrap().parent().unwrap());
    }

    /// O blob so abre com o sal do proprio cofre (entropia) e intacto.
    #[test]
    fn blob_is_bound_to_its_vault() {
        let file = temp_file("vinculo");
        let a = VaultKey::new("a").unwrap();
        let b = VaultKey::new("b").unwrap();
        enable(&file, &a).unwrap();

        // Blob de "a" copiado para a entrada de "b": o Windows recusa.
        let mut store = load(&file);
        let blob_a = store.vaults[0].key.clone();
        store.vaults.push(Entry {
            salt: hex(b.salt()),
            key: blob_a.clone(),
            suspended: false,
        });
        // Blob adulterado na entrada de "a".
        let mut bytes = unhex(&blob_a).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        store.vaults[0].key = hex(&bytes);
        save(&file, &store).unwrap();
        assert!(key_for(&file, b.salt()).is_err());
        assert!(key_for(&file, a.salt()).is_err());

        // Arquivo ilegivel conta como vazio.
        std::fs::write(&file, b"{nao e json").unwrap();
        assert_eq!(state(&file, a.salt()), State::Off);
        enable(&file, &a).unwrap();
        assert_eq!(state(&file, a.salt()), State::On);

        let _ = std::fs::remove_dir_all(file.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
        assert!(unhex("é").is_none());
    }
}
