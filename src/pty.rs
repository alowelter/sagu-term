//! Terminal local (cmd.exe) via PTY usando `portable-pty` (ConPTY no Windows).
//!
//! Reutiliza o mesmo protocolo da sessao SSH (`UiToSsh`/`SshToUi`) para que a UI
//! trate um terminal local exatamente como uma conexao remota.

use std::io::{Read, Write};
use std::sync::Arc;

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

use crate::ssh::{SshHandle, SshToUi, UiToSsh};

/// Abre um terminal local rodando `cmd.exe`. `repaint` acorda o loop do egui
/// sempre que ha novos bytes para exibir.
pub fn connect_cmd<F>(cols: u16, rows: u16, repaint: F) -> SshHandle
where
    F: Fn() + Send + Sync + 'static,
{
    let (to_pty_tx, mut to_pty_rx) = tokio::sync::mpsc::unbounded_channel::<UiToSsh>();
    let (from_pty_tx, from_pty_rx) = std::sync::mpsc::channel::<SshToUi>();

    let repaint = Arc::new(repaint);

    std::thread::spawn(move || {
        let fail = |msg: String| {
            let _ = from_pty_tx.send(SshToUi::Error(msg));
            (*repaint)();
            let _ = from_pty_tx.send(SshToUi::Closed);
            (*repaint)();
        };

        let pty_system = NativePtySystem::default();
        let pair = match pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => return fail(format!("pty: {e}")),
        };

        let mut cmd = CommandBuilder::new("cmd.exe");
        if let Ok(home) = std::env::var("USERPROFILE") {
            cmd.cwd(home);
        }

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => return fail(format!("cmd.exe: {e}")),
        };
        // O lado escravo nao e mais necessario depois do spawn.
        drop(pair.slave);

        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => return fail(format!("leitor: {e}")),
        };
        let mut writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => return fail(format!("escritor: {e}")),
        };

        let _ = from_pty_tx.send(SshToUi::Connected);
        (*repaint)();

        // Thread leitora: encaminha a saida do cmd.exe para a UI.
        let reader_tx = from_pty_tx.clone();
        let reader_repaint = Arc::clone(&repaint);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = reader_tx.send(SshToUi::Data(buf[..n].to_vec()));
                        (*reader_repaint)();
                    }
                }
            }
        });

        // Thread que aguarda o termino do cmd.exe (ex.: usuario digitou `exit`)
        // e sinaliza o encerramento. No Windows/ConPTY o leitor so recebe EOF
        // depois que o master e liberado, entao nao podemos depender do EOF do
        // leitor para detectar a saida — observamos o processo diretamente.
        let waiter_tx = from_pty_tx.clone();
        let waiter_repaint = Arc::clone(&repaint);
        let mut killer = child.clone_killer();
        std::thread::spawn(move || {
            let _ = child.wait();
            let _ = waiter_tx.send(SshToUi::Closed);
            (*waiter_repaint)();
        });

        // Loop de controle: recebe comandos da UI. `blocking_recv` funciona fora
        // de um contexto async (nao ha runtime tokio nesta thread).
        while let Some(cmd) = to_pty_rx.blocking_recv() {
            match cmd {
                UiToSsh::Data(data) => {
                    let _ = writer.write_all(&data);
                    let _ = writer.flush();
                }
                UiToSsh::Resize { cols, rows } => {
                    let _ = pair.master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                UiToSsh::Disconnect => break,
            }
        }

        let _ = killer.kill();
        // Ao sair, `pair.master` e `writer` sao liberados, desbloqueando o leitor.
    });

    SshHandle::from_parts(to_pty_tx, from_pty_rx)
}
