//! Instancia unica: abrir o SaguTerm com ele ja aberto (atalho de teclado,
//! menu Iniciar, barra de tarefas) traz a janela existente para a frente em
//! vez de abrir outra.
//!
//! Alem da comodidade, protege o cofre: cada janela grava o cofre inteiro que
//! tem na memoria, entao com duas abertas no mesmo cofre a ultima a salvar
//! apagaria o que a outra mudou (hosts novos, chaves de servidor aceitas).
//!
//! Um mapeamento de memoria nomeado faz os dois papeis: quem o cria e a
//! instancia principal e grava nele o proprio PID; quem chega depois recebe
//! ERROR_ALREADY_EXISTS, le o PID, acha a janela desse processo e a ativa.
//! O Windows apaga o objeto quando a instancia principal fecha, entao nao ha
//! trava velha para limpar depois de um travamento.

/// Como seguir depois de [`start`].
pub enum Start {
    /// Esta e a instancia principal; guardar ate o fim do programa.
    Primary(Slot),
    /// A janela ja aberta veio para a frente; este processo deve sair.
    Activated,
    /// Sem como garantir (falha da API ou a outra instancia nao mostrou a
    /// janela a tempo): abre normalmente, melhor duas janelas que nenhuma.
    Unchecked,
}

/// Titulo da janela principal. Sem a versao desde a 1.1.0 (ela fica no topo
/// da ajuda, F1); `imp` procura a janela por ele.
pub const WINDOW_TITLE: &str = "SaguTerm";

#[cfg(windows)]
pub use imp::{start, Slot};

#[cfg(not(windows))]
pub struct Slot;

#[cfg(not(windows))]
pub fn start() -> Start {
    Start::Unchecked
}

#[cfg(windows)]
mod imp {
    use super::Start;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// Nome por sessao do Windows ("Local\"). O build de depuracao usa outro,
    /// para `cargo run` nao esbarrar no app instalado.
    const NAME: &str = if cfg!(debug_assertions) {
        "Local\\SaguTerm.InstanciaUnica.Debug"
    } else {
        "Local\\SaguTerm.InstanciaUnica"
    };

    /// Quanto esperar a janela da outra instancia aparecer (ela pode estar
    /// abrindo agora mesmo).
    const WAIT: Duration = Duration::from_secs(3);

    /// Titulo ate a 1.0.1 ("SaguTerm v1.0.1"). Na troca de versao pela Store a
    /// janela da versao anterior pode seguir aberta: a nova ainda a reconhece.
    const OLD_TITLE_PREFIX: &str = "SaguTerm v";

    /// Titulo da janela principal: exatamente "SaguTerm", ou o antigo "SaguTerm v"
    /// seguido de um digito. Dialogos e janelas auxiliares do processo nao contam.
    fn is_main_title(title: &str) -> bool {
        title == super::WINDOW_TITLE
            || title
                .strip_prefix(OLD_TITLE_PREFIX)
                .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
    }

    type Handle = *mut c_void;
    type Hwnd = *mut c_void;

    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const PAGE_READWRITE: u32 = 0x04;
    const FILE_MAP_WRITE: u32 = 0x02;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const GW_OWNER: u32 = 4;
    const SW_RESTORE: i32 = 9;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileMappingW(
            file: Handle,
            attributes: *const c_void,
            protect: u32,
            size_high: u32,
            size_low: u32,
            name: *const u16,
        ) -> Handle;
        fn MapViewOfFile(map: Handle, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(view: *const c_void) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn GetLastError() -> u32;
    }

    #[link(name = "user32")]
    extern "system" {
        fn EnumWindows(visit: extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn IsIconic(hwnd: Hwnd) -> i32;
        fn GetWindowTextW(hwnd: Hwnd, text: *mut u16, max: i32) -> i32;
        fn ShowWindow(hwnd: Hwnd, cmd: i32) -> i32;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
    }

    /// Decide se este processo abre o app ou so ativa a janela ja aberta.
    pub fn start() -> Start {
        match Slot::open(NAME) {
            None => Start::Unchecked,
            Some((slot, false)) => {
                slot.pid().store(std::process::id(), Ordering::Release);
                Start::Primary(slot)
            }
            Some((slot, true)) if activate(&slot, WAIT) => Start::Activated,
            Some(_) => Start::Unchecked,
        }
    }

    /// Mapeamento nomeado de 4 bytes com o PID da instancia principal.
    pub struct Slot {
        handle: Handle,
        view: *mut u32,
    }

    impl Slot {
        /// Cria ou abre o objeto `name`; `true` junto quando ele ja existia.
        fn open(name: &str) -> Option<(Slot, bool)> {
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: nome terminado em zero; INVALID_HANDLE_VALUE pede memoria
            // do sistema (sem arquivo); o GetLastError vem logo em seguida.
            let (handle, existed) = unsafe {
                let h = CreateFileMappingW(INVALID_HANDLE_VALUE, null(), PAGE_READWRITE, 0, 4, wide.as_ptr());
                (h, GetLastError() == ERROR_ALREADY_EXISTS)
            };
            if handle.is_null() {
                return None;
            }
            // SAFETY: handle valido acima; a vista de 4 bytes cabe no objeto.
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, 4) } as *mut u32;
            // Montado antes do teste da vista: o Drop fecha o handle nos dois casos.
            let slot = Slot { handle, view };
            if view.is_null() {
                return None;
            }
            Some((slot, existed))
        }

        fn pid(&self) -> &AtomicU32 {
            // SAFETY: a vista (alinhada a pagina, 4 bytes) vive enquanto o Slot
            // vive e so e acessada atomicamente, aqui e na outra instancia.
            unsafe { AtomicU32::from_ptr(self.view) }
        }
    }

    impl Drop for Slot {
        fn drop(&mut self) {
            // SAFETY: vista e handle vieram de open() e sao liberados uma vez so.
            unsafe {
                if !self.view.is_null() {
                    UnmapViewOfFile(self.view as *const c_void);
                }
                CloseHandle(self.handle);
            }
        }
    }

    /// Espera a instancia principal publicar o PID e mostrar a janela, entao
    /// a restaura (se minimizada) e a traz para a frente. `false` se a janela
    /// nao apareceu dentro de `wait`.
    fn activate(slot: &Slot, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let pid = slot.pid().load(Ordering::Acquire);
            if let Some(hwnd) = (pid != 0).then(|| main_window(pid)).flatten() {
                // SAFETY: hwnd veio do EnumWindows; se a janela fechou nesse
                // meio tempo, as chamadas apenas falham.
                unsafe {
                    if IsIconic(hwnd) != 0 {
                        ShowWindow(hwnd, SW_RESTORE);
                    }
                    SetForegroundWindow(hwnd);
                }
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Janela principal do processo `pid`: de nivel superior, sem dono,
    /// visivel (inclusive minimizada) e com o titulo do SaguTerm (o atual ou
    /// o antigo com a versao, ver `is_main_title`).
    fn main_window(pid: u32) -> Option<Hwnd> {
        struct Search {
            pid: u32,
            found: Hwnd,
        }
        extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
            // SAFETY: lparam aponta para o Search de main_window, vivo durante
            // todo o EnumWindows; hwnd vem do proprio Windows.
            unsafe {
                let search = &mut *(lparam as *mut Search);
                let mut owner = 0u32;
                GetWindowThreadProcessId(hwnd, &mut owner);
                if owner != search.pid || !GetWindow(hwnd, GW_OWNER).is_null() || IsWindowVisible(hwnd) == 0 {
                    return 1;
                }
                let mut text = [0u16; 64];
                let n = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
                if is_main_title(&String::from_utf16_lossy(&text[..n])) {
                    search.found = hwnd;
                    return 0;
                }
                1
            }
        }
        let mut search = Search { pid, found: null_mut() };
        // SAFETY: o callback so usa o Search passado, que vive ate o retorno.
        unsafe { EnumWindows(visit, &mut search as *mut Search as isize) };
        (!search.found.is_null()).then_some(search.found)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_name(tag: &str) -> String {
            format!("Local\\SaguTerm.Teste.{tag}.{}", std::process::id())
        }

        #[test]
        fn second_open_sees_first_and_its_pid() {
            let name = test_name("pid");
            let (first, existed) = Slot::open(&name).expect("criar o objeto");
            assert!(!existed);
            first.pid().store(4242, Ordering::Release);
            let (second, existed) = Slot::open(&name).expect("abrir o objeto");
            assert!(existed);
            assert_eq!(second.pid().load(Ordering::Acquire), 4242);
            // Fechadas todas as referencias o objeto some: quem vier depois e
            // a principal de novo (nada de trava velha).
            drop((first, second));
            let (_again, existed) = Slot::open(&name).expect("recriar o objeto");
            assert!(!existed);
        }

        #[test]
        fn gives_up_when_no_window_shows_up() {
            // PID publicado mas sem janela do SaguTerm (o processo de teste nao
            // tem nenhuma): desiste no prazo e deixa o app abrir normalmente.
            let name = test_name("sem-janela");
            let (slot, _) = Slot::open(&name).expect("criar o objeto");
            slot.pid().store(std::process::id(), Ordering::Release);
            let t = Instant::now();
            assert!(!activate(&slot, Duration::from_millis(200)));
            assert!(t.elapsed() >= Duration::from_millis(200));
            assert!(main_window(std::process::id()).is_none());
        }

        #[test]
        fn waits_for_pid_before_searching() {
            // Objeto recem-criado pela outra instancia, PID ainda 0: nunca
            // procura janelas do "processo 0", so espera.
            let name = test_name("pid-zero");
            let (slot, _) = Slot::open(&name).expect("criar o objeto");
            assert_eq!(slot.pid().load(Ordering::Acquire), 0);
            assert!(!activate(&slot, Duration::from_millis(100)));
        }

        #[test]
        fn main_title_accepts_new_and_old() {
            // Titulo atual e o antigo com versao (janela da versao anterior
            // ainda aberta durante a troca pela Store).
            for t in ["SaguTerm", "SaguTerm v1.0.1", "SaguTerm v0.1.7"] {
                assert!(is_main_title(t), "{t:?}");
            }
            // Dialogos e outras janelas do processo nao sao a principal.
            for t in [
                "SaguTerm v",
                "SaguTerm vX",
                "SaguTerm - Abrir",
                "SaguTerm ",
                "saguterm",
                "Abrir",
                "Salvar como",
                "",
            ] {
                assert!(!is_main_title(t), "{t:?}");
            }
        }

        #[test]
        fn window_title_is_plain_name() {
            // Sem criar janela: gives_up_when_no_window_shows_up roda em
            // paralelo e conta com o processo de teste sem janelas.
            assert_eq!(super::super::WINDOW_TITLE, "SaguTerm");
            assert!(is_main_title(super::super::WINDOW_TITLE));
        }
    }
}
