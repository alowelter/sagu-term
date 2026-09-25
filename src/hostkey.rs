//! Verificacao da chave do servidor SSH (TOFU, estilo PuTTY).
//!
//! A chave aceita fica no proprio host do cofre (`Host::host_key`), no formato
//! OpenSSH "algoritmo base64" sem comentario. A sessao (SSH ou SFTP) compara a
//! chave apresentada com a guardada logo apos a troca de chaves e, se for nova
//! ou diferente, pergunta a UI (`HostKeyPrompt`) e espera a resposta ANTES de
//! enviar qualquer credencial (ver `ssh::connect_and_auth`).

use std::borrow::Cow;

use russh::keys::{Algorithm, HashAlg, PublicKey};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Resultado da comparacao entre a chave guardada e a apresentada.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCheck {
    /// Mesma chave ja aceita: conecta sem perguntar.
    Match,
    /// Nenhuma chave guardada: primeira conexao, pergunta.
    New,
    /// Chave diferente da guardada (ou guardada ilegivel): alerta.
    Changed,
}

/// Tipo e impressao digital de uma chave, para exibir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// Ex.: "ssh-ed25519".
    pub algorithm: String,
    /// "SHA256:<base64 sem padding>", igual ao `ssh-keygen -lf`.
    pub fingerprint: String,
}

/// Resposta da UI a uma pergunta de chave.
#[derive(Debug, PartialEq, Eq)]
pub enum HostKeyAnswer {
    /// Usuario confiou (ou a chave ja foi aceita por outro painel).
    Accept,
    /// Conexao abortada; o texto vai para o painel como erro.
    Cancel(String),
}

/// Pergunta enviada pela sessao a UI. Descartar sem responder (ex.: painel
/// fechado) aborta a conexao.
#[derive(Debug)]
pub struct HostKeyPrompt {
    /// Host do cofre (pelo id) onde a chave sera gravada se aceita.
    pub host_id: Uuid,
    /// Endereco e porta realmente usados nesta conexao.
    pub host: String,
    pub port: u16,
    /// Chave apresentada, forma OpenSSH sem comentario (`openssh_line`).
    pub presented: String,
    pub reply: oneshot::Sender<HostKeyAnswer>,
}

/// Forma OpenSSH "algoritmo base64" (sem comentario) da chave.
pub fn openssh_line(key: &PublicKey) -> anyhow::Result<String> {
    PublicKey::new(key.key_data().clone(), "")
        .to_openssh()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Le uma chave OpenSSH ("algoritmo base64 [comentario]").
fn parse(line: &str) -> Option<PublicKey> {
    PublicKey::from_openssh(line.trim()).ok()
}

/// Tipo e impressao digital SHA256 de uma chave OpenSSH; `None` se ilegivel.
pub fn describe(line: &str) -> Option<KeyInfo> {
    let key = parse(line)?;
    Some(KeyInfo {
        algorithm: key.algorithm().as_str().to_string(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    })
}

/// Compara pelo material da chave (KeyData), nao pelo texto: comentario e
/// espacos nao contam. Chave guardada ilegivel conta como `Changed` (alerta,
/// nunca silencio).
pub fn check(known: Option<&str>, presented: &str) -> KeyCheck {
    let Some(known) = known else {
        return KeyCheck::New;
    };
    match (parse(known), parse(presented)) {
        (Some(a), Some(b)) if a.key_data() == b.key_data() => KeyCheck::Match,
        _ => KeyCheck::Changed,
    }
}

/// Preferencia de algoritmos de chave do servidor: com chave guardada, o tipo
/// dela vem primeiro (como o OpenSSH faz com o known_hosts). Sem isso, um
/// servidor que ganhou uma chave ed25519 nova mostraria o alerta de troca com
/// a chave RSA antiga ainda valida. RSA: as tres variantes (sha512, sha256,
/// ssh-rsa) vao juntas para a frente, na ordem padrao. Sem chave ou tipo
/// desconhecido: `Preferred::default()`.
pub fn preferred(known: Option<&str>) -> russh::Preferred {
    let mut pref = russh::Preferred::default();
    let Some(alg) = known.and_then(parse).map(|k| k.algorithm()) else {
        return pref;
    };
    let rsa = matches!(alg, Algorithm::Rsa { .. });
    let (mut first, rest): (Vec<Algorithm>, Vec<Algorithm>) =
        pref.key.iter().cloned().partition(|a| {
            if rsa {
                matches!(a, Algorithm::Rsa { .. })
            } else {
                *a == alg
            }
        });
    if first.is_empty() {
        return pref;
    }
    first.extend(rest);
    pref.key = Cow::Owned(first);
    pref
}

/// Arquivo da chave publica no servidor OpenSSH, para a dica do dialogo:
/// "ssh-ed25519" -> "ssh_host_ed25519_key.pub", "ecdsa-sha2-*" ->
/// "ssh_host_ecdsa_key.pub", "ssh-rsa" -> "ssh_host_rsa_key.pub"; senao None.
pub fn server_pub_file(algorithm: &str) -> Option<&'static str> {
    match algorithm {
        "ssh-ed25519" => Some("ssh_host_ed25519_key.pub"),
        "ssh-rsa" => Some("ssh_host_rsa_key.pub"),
        a if a.starts_with("ecdsa-sha2-") => Some("ssh_host_ecdsa_key.pub"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::EcdsaCurve;

    // Vetores reais; impressoes conferidas com `ssh-keygen -lf`.
    const A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDx116/S6vbyAU3ZR1ebTYjMs187ZiPcltXd5Dg8Oapm teste-a";
    const A_FP: &str = "SHA256:JEpzgJ+qq0bLVo5Bj81AUoT0IRMtv5HFvtIy+xM6K74";
    const B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOaHfvKbIEav1XH7DfTNlEHNkxTAES3oEFcajJtkuuIU ";
    const B_FP: &str = "SHA256:ZaMQkuNWz1gMHIFCAGlBWlXZuF4Wq8cp7cWUk/lN8vo";
    const C: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHcFDdDBY2zlehteaLtk2gNc3ctQuOZ4f/UowOAv6oafNgVPW1HWS4jaMR0jOgvyMHCKYoIRRERsNvm4QkuhlVI=";
    const C_FP: &str = "SHA256:Pl24asKuVhii2nhk6bQMwg333Ude5yrd9bCtgqFbc8Y";
    const D: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQDUsNUJEdb01gzZ4tBYMf0qnZnc6a2VkFk8if5RKbF6HrfylWD8VBPhR9Cbqi2VExIaTsNWb7TWUvqau+V7KrvBARve1jYiYLbawfLsZNN7zWPGRonydq1sR4yw16xoguuJFAqegElslQmucdjUMgSVU4pPhovz4uEAAL/xE19WiQ==";
    const D_FP: &str = "SHA256:AiNRUK1tLNLpOJ2CLtjwQTjRpj9RAxovqypP4de9pDM";

    #[test]
    fn openssh_line_drops_comment_and_fingerprint_matches_ssh_keygen() {
        let line = openssh_line(&PublicKey::from_openssh(A).unwrap()).unwrap();
        assert_eq!(line.split(' ').count(), 2, "{line:?}");
        assert!(!line.contains("teste-a"));
        assert!(A.starts_with(&line));
        for (key, alg, fp) in [
            (A, "ssh-ed25519", A_FP),
            (B, "ssh-ed25519", B_FP),
            (C, "ecdsa-sha2-nistp256", C_FP),
            (D, "ssh-rsa", D_FP),
        ] {
            let info = describe(key).unwrap_or_else(|| panic!("ilegivel: {key}"));
            assert_eq!(info.algorithm, alg);
            assert_eq!(info.fingerprint, fp);
        }
        assert_eq!(describe("lixo"), None);
    }

    #[test]
    fn check_compares_key_material() {
        let a_bare = openssh_line(&PublicKey::from_openssh(A).unwrap()).unwrap();
        assert_eq!(check(None, &a_bare), KeyCheck::New);
        let a_noisy = format!("  {A}   ");
        assert_eq!(check(Some(&a_noisy), &a_bare), KeyCheck::Match);
        assert_eq!(check(Some(B), &a_bare), KeyCheck::Changed);
        assert_eq!(check(Some("lixo"), &a_bare), KeyCheck::Changed);
    }

    #[test]
    fn preferred_puts_known_type_first() {
        let default = russh::Preferred::default();
        assert_eq!(preferred(None).key, default.key);
        assert_eq!(preferred(Some("lixo")).key, default.key);

        assert_eq!(preferred(Some(A)).key[0], Algorithm::Ed25519);

        let c = preferred(Some(C)).key;
        let p256 = Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP256,
        };
        assert_eq!(c[0], p256);
        let rest: Vec<_> = default
            .key
            .iter()
            .filter(|a| **a != p256)
            .cloned()
            .collect();
        assert_eq!(&c[1..], &rest[..]);

        let d = preferred(Some(D)).key;
        assert_eq!(
            &d[..3],
            &[
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha512)
                },
                Algorithm::Rsa {
                    hash: Some(HashAlg::Sha256)
                },
                Algorithm::Rsa { hash: None },
            ]
        );
        assert_eq!(d.len(), default.key.len());
    }

    #[test]
    fn server_pub_file_hint() {
        assert_eq!(
            server_pub_file("ssh-ed25519"),
            Some("ssh_host_ed25519_key.pub")
        );
        assert_eq!(
            server_pub_file("ecdsa-sha2-nistp384"),
            Some("ssh_host_ecdsa_key.pub")
        );
        assert_eq!(server_pub_file("ssh-rsa"), Some("ssh_host_rsa_key.pub"));
        assert_eq!(server_pub_file("ssh-dss"), None);
    }

    // --- Ponta a ponta contra sshd reais (ignorados) -----------------------
    //
    // Mesmas variaveis dos testes de `upload`: SAGU_E2E_PORT, SAGU_E2E_USER,
    // SAGU_E2E_KEY e SAGU_E2E_ROOT. O teste de troca e o de queda usam tambem
    // SAGU_E2E_PORT_B: um segundo sshd com OUTRA HostKey e `LoginGraceTime 5`.
    // Rodar com: cargo test e2e_host_key -- --ignored --test-threads=1

    use crate::sftp::{self, SftpHandle, SftpToUi};
    use crate::ssh::{self, SshHandle, SshToUi};
    use crate::vault::{AuthMethod, Host};
    use std::time::{Duration, Instant};

    const INTERRUPTED: &str =
        "Conexão cancelada: a confirmação da chave do servidor foi interrompida.";

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"))
    }

    fn env_port(k: &str) -> u16 {
        env(k)
            .parse()
            .unwrap_or_else(|_| panic!("{k} nao e uma porta"))
    }

    /// Host de teste (127.0.0.1) com a chave de cliente do sshd descartavel.
    fn e2e_host(port: u16) -> Host {
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = port;
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        host
    }

    /// Proximo evento da sessao SSH (ignora a saida do terminal).
    fn next_ev(h: &SshHandle, secs: u64) -> Option<SshToUi> {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            match h.from_ssh.recv_timeout(Duration::from_millis(100)) {
                Ok(SshToUi::Data(_)) => {}
                Ok(ev) => return Some(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn expect_prompt(h: &SshHandle) -> HostKeyPrompt {
        match next_ev(h, 15) {
            Some(SshToUi::HostKey(p)) => p,
            Some(SshToUi::Connected) => panic!("conectou sem perguntar a chave"),
            Some(SshToUi::Error(e)) => panic!("erro antes da pergunta: {e}"),
            Some(SshToUi::Closed) => panic!("sessao fechou antes da pergunta"),
            Some(SshToUi::Upload(u)) => panic!("evento inesperado: {u:?}"),
            Some(SshToUi::Data(_)) => unreachable!(),
            None => panic!("tempo esgotado esperando a pergunta da chave"),
        }
    }

    fn expect_connected(h: &SshHandle) {
        match next_ev(h, 15) {
            Some(SshToUi::Connected) => {}
            Some(SshToUi::HostKey(p)) => panic!("perguntou a chave sem motivo: {}", p.presented),
            Some(SshToUi::Error(e)) => panic!("erro ao conectar: {e}"),
            Some(SshToUi::Closed) => panic!("sessao fechou sem conectar"),
            Some(SshToUi::Upload(u)) => panic!("evento inesperado: {u:?}"),
            Some(SshToUi::Data(_)) => unreachable!(),
            None => panic!("tempo esgotado esperando a conexao"),
        }
    }

    /// Erro da sessao (texto), seguido obrigatoriamente de `Closed`.
    fn expect_error(h: &SshHandle, secs: u64) -> String {
        let msg = match next_ev(h, secs) {
            Some(SshToUi::Error(e)) => e,
            Some(SshToUi::Connected) => panic!("conectou apesar da recusa"),
            Some(SshToUi::HostKey(_)) => panic!("perguntou de novo"),
            Some(SshToUi::Closed) => panic!("fechou sem erro"),
            Some(SshToUi::Upload(u)) => panic!("evento inesperado: {u:?}"),
            Some(SshToUi::Data(_)) => unreachable!(),
            None => panic!("tempo esgotado esperando o erro"),
        };
        assert!(
            matches!(next_ev(h, 5), Some(SshToUi::Closed)),
            "sem Closed apos o erro"
        );
        msg
    }

    /// Encerra pelo protocolo (nunca digitando `exit`) e espera o `Closed`.
    fn disconnect(h: &SshHandle) {
        h.disconnect();
        match next_ev(h, 10) {
            Some(SshToUi::Closed) => {}
            Some(SshToUi::Error(e)) => panic!("erro ao desconectar: {e}"),
            _ => panic!("sessao nao fechou"),
        }
    }

    fn ssh_connect(host: &Host) -> SshHandle {
        ssh::connect(host.clone(), 80, 24, || {})
    }

    #[test]
    #[ignore]
    fn e2e_host_key_tofu_ssh() {
        let port = env_port("SAGU_E2E_PORT");
        let mut host = e2e_host(port);

        // a) Servidor novo: pergunta com os dados da conexao; recusar aborta.
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(p.host_id, host.id);
        assert_eq!((p.host.as_str(), p.port), ("127.0.0.1", port));
        assert_eq!(check(None, &p.presented), KeyCheck::New);
        let info = describe(&p.presented).expect("chave apresentada ilegivel");
        assert_eq!(info.algorithm, "ssh-ed25519");
        let pub_file = std::path::PathBuf::from(env("SAGU_E2E_ROOT"))
            .join("home")
            .join(env("SAGU_E2E_USER"))
            .join("sagu-e2e")
            .join("host_ed25519.pub");
        if let Ok(text) = std::fs::read_to_string(&pub_file) {
            assert_eq!(describe(&text).unwrap().fingerprint, info.fingerprint);
        }
        let key = p.presented.clone();
        p.reply
            .send(HostKeyAnswer::Cancel("teste: recusada".into()))
            .unwrap();
        assert_eq!(expect_error(&h, 10), "teste: recusada");

        // b) Pergunta descartada sem resposta (ex.: painel fechado): aborta.
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(p.presented, key);
        drop(p);
        assert_eq!(expect_error(&h, 10), INTERRUPTED);

        // c) Aceitar conecta.
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        p.reply.send(HostKeyAnswer::Accept).unwrap();
        expect_connected(&h);
        disconnect(&h);

        // d) Chave ja guardada: conecta sem perguntar.
        host.host_key = Some(key.clone());
        let h = ssh_connect(&host);
        expect_connected(&h);
        disconnect(&h);

        // e) Chave guardada diferente: alerta de troca; cancelar aborta.
        host.host_key = Some(B.to_string());
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(
            check(host.host_key.as_deref(), &p.presented),
            KeyCheck::Changed
        );
        assert_eq!(p.presented, key);
        p.reply
            .send(HostKeyAnswer::Cancel("teste: mudou".into()))
            .unwrap();
        assert_eq!(expect_error(&h, 10), "teste: mudou");

        // f) Guardada ECDSA, servidor so com ed25519: a preferencia pela
        // ECDSA nao impede a negociacao; chega o alerta de troca.
        host.host_key = Some(C.to_string());
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(
            check(host.host_key.as_deref(), &p.presented),
            KeyCheck::Changed
        );
        assert_eq!(p.presented, key);
        p.reply
            .send(HostKeyAnswer::Cancel("teste: ecdsa".into()))
            .unwrap();
        assert_eq!(expect_error(&h, 10), "teste: ecdsa");
    }

    /// Proximo evento da sessao SFTP.
    fn next_sftp(h: &SftpHandle, secs: u64) -> Option<SftpToUi> {
        h.from_sftp.recv_timeout(Duration::from_secs(secs)).ok()
    }

    fn sftp_connect(host: &Host) -> SftpHandle {
        sftp::connect(host.clone(), || {})
    }

    fn expect_sftp_connected(h: &SftpHandle) {
        match next_sftp(h, 15) {
            Some(SftpToUi::Connected { home }) => assert!(!home.is_empty()),
            Some(SftpToUi::HostKey(p)) => panic!("perguntou a chave sem motivo: {}", p.presented),
            Some(SftpToUi::Error(e)) => panic!("erro ao conectar: {e}"),
            _ => panic!("SFTP nao conectou"),
        }
        h.disconnect();
        assert!(matches!(next_sftp(h, 10), Some(SftpToUi::Closed)));
    }

    #[test]
    #[ignore]
    fn e2e_host_key_tofu_sftp() {
        let mut host = e2e_host(env_port("SAGU_E2E_PORT"));

        // Servidor novo: pergunta; aceitar conecta e mostra a pasta pessoal.
        let h = sftp_connect(&host);
        let p = match next_sftp(&h, 15) {
            Some(SftpToUi::HostKey(p)) => p,
            _ => panic!("SFTP nao perguntou a chave"),
        };
        assert_eq!(check(None, &p.presented), KeyCheck::New);
        let key = p.presented.clone();
        p.reply.send(HostKeyAnswer::Accept).unwrap();
        expect_sftp_connected(&h);

        // Chave guardada: conecta sem perguntar.
        host.host_key = Some(key);
        expect_sftp_connected(&sftp_connect(&host));

        // Chave guardada diferente: pergunta; cancelar aborta.
        host.host_key = Some(B.to_string());
        let h = sftp_connect(&host);
        let p = match next_sftp(&h, 15) {
            Some(SftpToUi::HostKey(p)) => p,
            _ => panic!("SFTP nao alertou a troca"),
        };
        assert_eq!(
            check(host.host_key.as_deref(), &p.presented),
            KeyCheck::Changed
        );
        p.reply
            .send(HostKeyAnswer::Cancel("teste: sftp".into()))
            .unwrap();
        match next_sftp(&h, 10) {
            Some(SftpToUi::Error(e)) => assert_eq!(e, "teste: sftp"),
            _ => panic!("esperava o erro do cancelamento"),
        }
        assert!(matches!(next_sftp(&h, 5), Some(SftpToUi::Closed)));
    }

    /// Mesmo host "reinstalado": SAGU_E2E_PORT_B serve outra HostKey.
    #[test]
    #[ignore]
    fn e2e_host_key_swapped_on_server() {
        let port_a = env_port("SAGU_E2E_PORT");
        let port_b = env_port("SAGU_E2E_PORT_B");
        let mut host = e2e_host(port_a);

        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        let key_a = p.presented.clone();
        p.reply.send(HostKeyAnswer::Accept).unwrap();
        expect_connected(&h);
        disconnect(&h);

        // Mesmo host do cofre, agora servindo a chave B: alerta de troca. (O
        // editor apagaria a chave ao trocar a porta; aqui o teste e do
        // protocolo, entao o host e montado direto.)
        host.port = port_b;
        host.host_key = Some(key_a.clone());
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(check(Some(&key_a), &p.presented), KeyCheck::Changed);
        let key_b = p.presented.clone();
        assert_ne!(key_b, key_a);
        p.reply
            .send(HostKeyAnswer::Cancel("teste: troca recusada".into()))
            .unwrap();
        assert_eq!(expect_error(&h, 10), "teste: troca recusada");

        // Aceitar a nova chave conecta.
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(p.presented, key_b);
        p.reply.send(HostKeyAnswer::Accept).unwrap();
        expect_connected(&h);
        disconnect(&h);

        // Com a chave B guardada: sem pergunta na B; a A volta a alertar.
        host.host_key = Some(key_b.clone());
        let h = ssh_connect(&host);
        expect_connected(&h);
        disconnect(&h);
        host.port = port_a;
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        assert_eq!(p.presented, key_a);
        assert_eq!(check(Some(&key_b), &p.presented), KeyCheck::Changed);
        p.reply
            .send(HostKeyAnswer::Cancel("teste: volta".into()))
            .unwrap();
        assert_eq!(expect_error(&h, 10), "teste: volta");
    }

    /// O servidor derruba a conexao (LoginGraceTime 5 no sshd B) enquanto a
    /// pergunta esta aberta: a sessao desiste sozinha, com mensagem clara.
    #[test]
    #[ignore]
    fn e2e_host_key_server_drops_while_asking() {
        let host = e2e_host(env_port("SAGU_E2E_PORT_B"));
        let h = ssh_connect(&host);
        let p = expect_prompt(&h);
        let msg = expect_error(&h, 12);
        assert!(
            msg.contains("O servidor encerrou a conexão enquanto aguardava"),
            "{msg}"
        );
        assert!(p.reply.is_closed(), "a pergunta deveria ter caido");
    }
}
