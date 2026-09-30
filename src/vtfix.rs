//! Traducao do fluxo do servidor antes do parser `vt100`: sequencias de
//! cursor que ele ignora viram as equivalentes que ele conhece.
//!
//! O vt100 0.16 trata `CSI linha;coluna H` (CUP) e `ESC 7`/`ESC 8`
//! (DECSC/DECRC), mas ignora tres sequencias que o btop usa em todo quadro:
//! - `CSI linha;coluna f` (HVP): o mesmo que o CUP. O btop posiciona tudo com
//!   ela (o htop, pelo ncurses, usa o CUP); sem ela o texto sai corrido,
//!   quebrando nas bordas;
//! - `CSI s` / `CSI u` (SCOSC/SCORC): salvar/restaurar o cursor, como o
//!   DECSC/DECRC.
//!
//! As rotinas do vt100 que resolveriam (`Screen::cup`, `decsc`, `decrc`) sao
//! internas ao crate, entao a correcao e feita no fluxo:
//! - `CSI <params> f` sem marcador privado (`<=>?`) nem intermediarios vira
//!   `CSI <params> H` (so o byte final muda);
//! - `CSI s` e `CSI u` exatos (sem parametros, marcador nem intermediarios)
//!   viram `ESC 7` e `ESC 8`. Com parametros ficam como estao: sao outras
//!   sequencias (DECSLRM `CSI 1;80 s`, teclado do kitty `CSI ? u`,
//!   `CSI > 1 u`, `CSI = 1;1 u`...);
//! - todo o resto passa identico, byte a byte.
//!
//! O tradutor tambem avisa (`Piece::AltScreen`) logo depois de cada
//! `CSI ? 1049 h`/`l` exato (so esse parametro), que entra/sai da tela
//! alternativa: o vt100 guarda um so conjunto de atributos salvos para as
//! duas telas, e um `ESC 7` (ou `CSI s`) na alternativa apagaria as cores que
//! o `1049 h` salvou da principal. O `Terminal` guarda a copia e a devolve.
//!
//! O reconhecimento imita a maquina de estados do vte 0.15 (o parser por
//! baixo do vt100), para traduzir exatamente o que o vt100 despacharia como
//! CSI:
//! - ESC, em qualquer estado, comeca um escape novo (encerra strings e aborta
//!   sequencias); CAN e SUB abortam e voltam ao texto;
//! - dentro de strings (OSC, DCS, SOS/PM/APC) so o ESC comeca algo novo (CAN,
//!   SUB e, no OSC, o BEL as encerram), entao para a traducao elas contam
//!   como texto: um "[f" no titulo da janela nao muda;
//! - dentro do escape e do CSI os controles C0 sao executados sem
//!   interromper a sequencia, e DEL e bytes >= 0x80 sao ignorados: o tradutor
//!   os repassa na mesma ordem e a sequencia continua valendo;
//! - so codigos de 7 bits: 0x9B nao e CSI (o vte tambem nao o trata).
//!
//! Os bytes chegam em pedacos arbitrarios: o estado atravessa as chamadas e
//! so ficam segurados, ate o pedaco seguinte:
//! - o '[' de um "ESC [" no fim do pedaco (para ver se vem 's' ou 'u');
//! - um caractere UTF-8 incompleto no fim do texto (ate 3 bytes). O vte 0.15
//!   perde bytes quando recebe o resto de um caractere junto com mais texto
//!   (`advance_partial_utf8` avanca pelos bytes validos do buffer, nao pelos
//!   do caractere): "45°C│" cortado no meio do ° vira "45°│", e um ESC
//!   seguido de byte alto some (o tradutor perderia o passo). Segurado, o
//!   caractere vai inteiro num trecho so e o vte nunca o recebe partido.

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;
const BEL: u8 = 0x07;

/// O que o tradutor entrega, em ordem, a quem usa o parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Bytes para o parser (nunca vazio).
    Bytes(&'a [u8]),
    /// O trecho anterior foi so o byte final de um `CSI ? 1049 h` (`true`,
    /// entra na tela alternativa) ou `CSI ? 1049 l` (`false`, sai dela).
    AltScreen(bool),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    /// Texto: so o ESC importa.
    #[default]
    Text,
    /// Conteudo de string (OSC, DCS, SOS/PM/APC). Para a traducao e como o
    /// texto (so o ESC comeca algo novo), mas o vte nao esta no estado base
    /// (ver `idle`). Sai no CAN/SUB e, no OSC (`osc`), tambem no BEL. O
    /// DCS tambem acaba no 0x9C depois do byte final dele; aqui ele so
    /// acaba no proximo ESC/CAN/SUB (`idle` fica falso a mais, nunca a menos).
    Str { osc: bool },
    /// Logo depois de um ESC.
    Esc,
    /// ESC e intermediario(s) (0x20..=0x2f, ex. o "ESC (" do sgr0): falta o
    /// byte final, e um '[' aqui e final, nao abre CSI.
    EscInter,
    /// "ESC [" no fim do pedaco anterior: o '[' ainda nao foi repassado.
    CsiStart,
    /// Dentro de um CSI (o "ESC [" ja foi repassado).
    Csi(Csi),
}

/// O que ja apareceu dentro de um CSI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Csi {
    /// Nada que conte (so controles, DEL ou bytes >= 0x80).
    Empty,
    /// Parametros: digitos, ':' e ';'.
    Params,
    /// '?' no inicio e depois so digitos: o valor do parametro, contado como
    /// no vte (saturando), para achar o `CSI ? 1049 h`/`l`.
    Dec(u16),
    /// Marcador privado, intermediario ou sequencia que o vte descarta:
    /// nunca traduz.
    Other,
}

/// Controle C0 que o vte executa sem sair do escape ou do CSI (todos menos
/// CAN, SUB e ESC).
fn is_exec(b: u8) -> bool {
    matches!(b, 0x00..=0x17 | 0x19 | 0x1c..=0x1f)
}

/// Byte de continuacao do UTF-8 (10xxxxxx).
fn is_cont(b: u8) -> bool {
    b & 0xc0 == 0x80
}

/// Tamanho (0 a 3) do caractere UTF-8 incompleto no fim de `s`: o comeco
/// valido de um caractere cujo resto ainda nao chegou, pelo criterio do vte
/// (`str::from_utf8` so reclama do fim da entrada).
fn utf8_tail(s: &[u8]) -> usize {
    // O caractere comeca no ultimo byte que nao e de continuacao.
    let Some(back) = s.iter().rev().take(3).position(|&b| !is_cont(b)) else {
        return 0;
    };
    let tail = &s[s.len() - 1 - back..];
    match std::str::from_utf8(tail) {
        Err(e) if e.valid_up_to() == 0 && e.error_len().is_none() => tail.len(),
        _ => 0,
    }
}

/// Tradutor com estado; um por terminal (ver `Terminal::process`).
#[derive(Debug, Default)]
pub struct VtFix {
    state: State,
    /// Caractere UTF-8 incompleto do fim do ultimo pedaco (so no texto).
    held: [u8; 3],
    held_len: u8,
}

impl VtFix {
    /// Nada pendente: o vte no estado base (fora de escape, CSI e strings) e
    /// sem caractere UTF-8 segurado. O `Terminal` so injeta bytes proprios no
    /// parser (ao encolher a tela) neste estado: o ESC deles abortaria a
    /// sequencia do servidor, e o resto dela iria para a tela.
    pub fn idle(&self) -> bool {
        self.state == State::Text && self.held_len == 0
    }

    /// Traduz `input` e entrega o resultado a `out`, em trechos: fatias do
    /// proprio `input` (sem copia) intercaladas com as trocas e os avisos. A
    /// saida de pedacos seguidos e a mesma de uma chamada so com tudo junto.
    pub fn feed(&mut self, input: &[u8], mut out: impl FnMut(Piece<'_>)) {
        macro_rules! emit {
            ($s:expr) => {{
                let s: &[u8] = $s;
                if !s.is_empty() {
                    out(Piece::Bytes(s));
                }
            }};
        }
        // Inicio do trecho de `input` ainda nao entregue.
        let mut start = 0;
        let mut i = 0;
        if self.held_len > 0 {
            // Completa o caractere segurado com as continuacoes que chegaram.
            let mut buf = [0; 4];
            let mut n = usize::from(self.held_len);
            buf[..n].copy_from_slice(&self.held[..n]);
            while utf8_tail(&buf[..n]) == n {
                match input.get(i) {
                    Some(&b) if is_cont(b) => {
                        buf[n] = b;
                        n += 1;
                        i += 1;
                    }
                    _ => break,
                }
            }
            if i == input.len() && utf8_tail(&buf[..n]) == n {
                // Ainda incompleto e o pedaco acabou: continua segurado.
                self.held[..n].copy_from_slice(&buf[..n]);
                self.held_len = n as u8;
                return;
            }
            // Inteiro num trecho so. Se nao completou (veio outro byte), vai
            // sozinho: o vte o troca por U+FFFD sem perder o byte seguinte.
            emit!(&buf[..n]);
            self.held_len = 0;
            start = i;
        }
        while i < input.len() {
            let b = input[i];
            match self.state {
                State::Text => {
                    // Caminho rapido: pula direto ao proximo ESC.
                    match input[i..].iter().position(|&c| c == ESC) {
                        Some(n) => {
                            i += n;
                            self.state = State::Esc;
                        }
                        None => break,
                    }
                }
                State::Str { osc } => {
                    // Caminho rapido: pula ao proximo byte que encerra a string.
                    let end = |c: u8| c == ESC || c == CAN || c == SUB || (osc && c == BEL);
                    match input[i..].iter().position(|&c| end(c)) {
                        Some(n) => {
                            i += n;
                            self.state = if input[i] == ESC {
                                State::Esc
                            } else {
                                State::Text
                            };
                        }
                        None => break,
                    }
                }
                State::Esc => {
                    self.state = match b {
                        b'[' => match input.get(i + 1) {
                            Some(&c @ (b's' | b'u')) => {
                                // "ESC [ s" vira "ESC 7" ("ESC [ u", "ESC 8"):
                                // o ESC vai no trecho, o "[s" fica de fora.
                                emit!(&input[start..i]);
                                emit!(if c == b's' { b"7" } else { b"8" });
                                i += 1;
                                start = i + 1;
                                State::Text
                            }
                            // Outro CSI: segue no mesmo trecho, sem corte.
                            Some(_) => State::Csi(Csi::Empty),
                            None => {
                                // Fim do pedaco: segura o '[' ate ver o
                                // proximo byte.
                                emit!(&input[start..i]);
                                start = i + 1;
                                State::CsiStart
                            }
                        },
                        // Controle executado, outro ESC, DEL ou >= 0x80: o
                        // escape continua.
                        _ if is_exec(b) || b == ESC || b >= 0x7f => State::Esc,
                        // Intermediario: falta o byte final.
                        0x20..=0x2f => State::EscInter,
                        // Inicio de string: OSC; DCS, SOS, PM e APC.
                        b']' => State::Str { osc: true },
                        b'P' | b'X' | b'^' | b'_' => State::Str { osc: false },
                        // Escape completo, ou CAN/SUB (abortam).
                        _ => State::Text,
                    };
                }
                State::EscInter => {
                    self.state = match b {
                        ESC => State::Esc,
                        // Byte final (inclusive '['): o vte despacha o escape.
                        CAN | SUB | 0x30..=0x7e => State::Text,
                        // Controle executado, mais intermediarios, DEL ou
                        // >= 0x80: continua esperando o final.
                        _ => State::EscInter,
                    };
                }
                State::CsiStart => {
                    // So no comeco de um pedaco: nada de `input` ficou pendente.
                    debug_assert_eq!((start, i), (0, 0));
                    match b {
                        b's' | b'u' => {
                            // O ESC ja foi; falta so o 7/8.
                            emit!(if b == b's' { b"7" } else { b"8" });
                            start = i + 1;
                            self.state = State::Text;
                        }
                        _ => {
                            // Outro CSI: devolve o '[' e trata o byte dentro dele.
                            emit!(b"[");
                            self.state = State::Csi(Csi::Empty);
                            continue;
                        }
                    }
                }
                State::Csi(kind) => {
                    self.state = match b {
                        // Byte final: o vte despacha (ou descarta, se Other).
                        0x40..=0x7e => {
                            let swap: Option<&[u8]> = match (b, kind) {
                                (b'f', Csi::Empty | Csi::Params) => Some(b"H"),
                                // Houve controle/DEL entre o '[' e o final (o
                                // "ESC [" ja foi repassado): o ESC aborta esse
                                // CSI, sem efeito no vte, e comeca o ESC 7/8.
                                (b's', Csi::Empty) => Some(b"\x1b7"),
                                (b'u', Csi::Empty) => Some(b"\x1b8"),
                                _ => None,
                            };
                            if let Some(swap) = swap {
                                emit!(&input[start..i]);
                                emit!(swap);
                                start = i + 1;
                            } else if kind == Csi::Dec(1049) && matches!(b, b'h' | b'l') {
                                // O byte final sozinho num trecho e, logo
                                // depois dele, o aviso.
                                emit!(&input[start..i]);
                                emit!(&input[i..=i]);
                                out(Piece::AltScreen(b == b'h'));
                                start = i + 1;
                            }
                            State::Text
                        }
                        b'0'..=b'9' => State::Csi(match kind {
                            Csi::Empty => Csi::Params,
                            Csi::Dec(n) => {
                                Csi::Dec(n.saturating_mul(10).saturating_add(u16::from(b - b'0')))
                            }
                            other => other,
                        }),
                        b':' | b';' => State::Csi(match kind {
                            Csi::Empty | Csi::Params => Csi::Params,
                            // Mais de um parametro (ou subparametro): nao e
                            // o 1049 exato.
                            Csi::Dec(_) | Csi::Other => Csi::Other,
                        }),
                        b'?' if kind == Csi::Empty => State::Csi(Csi::Dec(0)),
                        0x20..=0x2f | 0x3c..=0x3f => State::Csi(Csi::Other),
                        CAN | SUB => State::Text,
                        ESC => State::Esc,
                        // Controle executado, DEL ou >= 0x80: nada muda.
                        _ => State::Csi(kind),
                    };
                }
            }
            i += 1;
        }
        // Fim do pedaco; no texto (e nas strings, que antes contavam como
        // texto), o caractere UTF-8 incompleto fica segurado.
        let rest = &input[start..];
        let keep = if matches!(self.state, State::Text | State::Str { .. }) {
            utf8_tail(rest)
        } else {
            0
        };
        emit!(&rest[..rest.len() - keep]);
        self.held[..keep].copy_from_slice(&rest[rest.len() - keep..]);
        self.held_len = keep as u8;
    }

    /// Bytes segurados ate o proximo pedaco: o '[' de um "ESC [" ou o
    /// caractere UTF-8 incompleto do fim.
    #[cfg(test)]
    fn pending(&self) -> Vec<u8> {
        if self.state == State::CsiStart {
            b"[".to_vec()
        } else {
            self.held[..usize::from(self.held_len)].to_vec()
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Aviso de tela alternativa: (bytes entregues ate ele, entrou?).
    type Notice = (usize, bool);

    /// O que o tradutor entregou, para comparar.
    #[derive(Debug, Default, PartialEq)]
    struct Fixed {
        /// Bytes entregues, na ordem.
        out: Vec<u8>,
        /// Bytes segurados no fim.
        pending: Vec<u8>,
        /// Avisos de tela alternativa.
        alt: Vec<Notice>,
    }

    /// Recebe os trechos e confere as regras de entrega: nenhum trecho vazio
    /// e o aviso logo depois do byte final, sozinho no trecho anterior.
    #[derive(Default)]
    struct Sink {
        fixed: Fixed,
        last: Option<Vec<u8>>,
    }

    impl Sink {
        fn take(&mut self, piece: Piece<'_>) {
            match piece {
                Piece::Bytes(s) => {
                    assert!(!s.is_empty(), "trecho vazio");
                    self.fixed.out.extend_from_slice(s);
                    self.last = Some(s.to_vec());
                }
                Piece::AltScreen(on) => {
                    let fin: &[u8] = if on { b"h" } else { b"l" };
                    assert_eq!(
                        self.last.take().as_deref(),
                        Some(fin),
                        "aviso fora do lugar"
                    );
                    self.fixed.alt.push((self.fixed.out.len(), on));
                }
            }
        }
    }

    /// Traduz `input` cortado nos pontos `cuts` (crescentes), no mesmo tradutor.
    fn fix_cut(input: &[u8], cuts: &[usize]) -> Fixed {
        let mut f = VtFix::default();
        let mut sink = Sink::default();
        let mut from = 0;
        for &c in cuts.iter().chain(std::iter::once(&input.len())) {
            f.feed(&input[from..c], |p| sink.take(p));
            from = c;
        }
        sink.fixed.pending = f.pending();
        sink.fixed
    }

    /// Traduz `input` de uma vez.
    fn fix(input: &[u8]) -> Fixed {
        fix_cut(input, &[])
    }

    /// Mesmo resultado com a entrada cortada em todos os pontos (um corte;
    /// dois cortes se `pairs`; e byte a byte).
    fn assert_cut_invariant(input: &[u8], pairs: bool) {
        let whole = fix(input);
        for a in 0..=input.len() {
            assert_eq!(fix_cut(input, &[a]), whole, "corte em {a}: {input:?}");
            if pairs {
                for b in a..=input.len() {
                    assert_eq!(fix_cut(input, &[a, b]), whole, "cortes {a},{b}: {input:?}");
                }
            }
        }
        let every: Vec<usize> = (1..input.len()).collect();
        assert_eq!(fix_cut(input, &every), whole, "byte a byte: {input:?}");
    }

    /// Um item entregue pelo tradutor.
    #[derive(Debug, PartialEq)]
    enum Item {
        B(Vec<u8>),
        Alt(bool),
    }

    /// Os trechos e avisos, na ordem, com os pedacos dados.
    fn items(chunks: &[&[u8]]) -> Vec<Item> {
        let mut f = VtFix::default();
        let mut v = Vec::new();
        for c in chunks {
            f.feed(c, |p| {
                v.push(match p {
                    Piece::Bytes(s) => Item::B(s.to_vec()),
                    Piece::AltScreen(on) => Item::Alt(on),
                })
            });
        }
        v
    }

    fn b(s: &[u8]) -> Item {
        Item::B(s.to_vec())
    }

    /// (entrada, saida esperada)
    const CASES: &[(&[u8], &[u8])] = &[
        // HVP vira CUP, com ou sem parametros.
        (b"\x1b[5;10f", b"\x1b[5;10H"),
        (b"\x1b[f", b"\x1b[H"),
        (b"\x1b[0;0f", b"\x1b[0;0H"),
        (b"\x1b[;7f", b"\x1b[;7H"),
        (b"\x1b[12f", b"\x1b[12H"),
        (b"\x1b[5:1;10f", b"\x1b[5:1;10H"),
        (b"\x1b[99999;99999f", b"\x1b[99999;99999H"),
        (b"ab\x1b[2;3fcd\x1b[4;5fef", b"ab\x1b[2;3Hcd\x1b[4;5Hef"),
        // Salvar/restaurar exatos.
        (b"\x1b[s", b"\x1b7"),
        (b"\x1b[u", b"\x1b8"),
        (b"x\x1b[sy\x1b[uz", b"x\x1b7y\x1b8z"),
        // Com marcador privado, intermediario ou parametros: fica igual.
        (b"\x1b[?5f", b"\x1b[?5f"),
        (b"\x1b[>5f", b"\x1b[>5f"),
        (b"\x1b[=f", b"\x1b[=f"),
        (b"\x1b[<1;2f", b"\x1b[<1;2f"),
        (b"\x1b[5 f", b"\x1b[5 f"),
        (b"\x1b[!f", b"\x1b[!f"),
        (b"\x1b[5;?f", b"\x1b[5;?f"),
        (b"\x1b[1;80s", b"\x1b[1;80s"),
        (b"\x1b[0s", b"\x1b[0s"),
        (b"\x1b[;s", b"\x1b[;s"),
        (b"\x1b[?u", b"\x1b[?u"),
        (b"\x1b[>1u", b"\x1b[>1u"),
        (b"\x1b[=1;1u", b"\x1b[=1;1u"),
        (b"\x1b[<u", b"\x1b[<u"),
        (b"\x1b[1u", b"\x1b[1u"),
        (b"\x1b[ s", b"\x1b[ s"),
        (b"\x1b[?1049s", b"\x1b[?1049s"),
        // Outras sequencias e letras soltas.
        (b"fsu \x1b[31mfus\x1b[0m", b"fsu \x1b[31mfus\x1b[0m"),
        (
            b"\x1b[?1049h\x1b[?25l\x1b[2J",
            b"\x1b[?1049h\x1b[?25l\x1b[2J",
        ),
        (b"\x1b7\x1b8\x1bM\x1bc", b"\x1b7\x1b8\x1bM\x1bc"),
        // Controles dentro do CSI: repassados na ordem, a sequencia vale.
        (b"\x1b[5\r;10f", b"\x1b[5\r;10H"),
        (b"\x1b[\rs", b"\x1b[\r\x1b7"),
        (b"\x1b[\x07\x08u", b"\x1b[\x07\x08\x1b8"),
        (b"\x1b[\x19\x1c\x1fs", b"\x1b[\x19\x1c\x1f\x1b7"),
        (b"\x1b[2\x19;3f", b"\x1b[2\x19;3H"),
        (b"\x1b[\x7fs", b"\x1b[\x7f\x1b7"),
        (b"\x1b[\x80\xffu", b"\x1b[\x80\xff\x1b8"),
        (b"\x1b[\rf", b"\x1b[\rH"),
        (b"\x1b[\r1s", b"\x1b[\r1s"),
        // Controles entre o ESC e o '[' (o vte os executa no escape).
        (b"\x1b\r[s", b"\x1b\r7"),
        (b"\x1b\x7f[5f", b"\x1b\x7f[5H"),
        (b"\x1b\x19\x00[s", b"\x1b\x19\x007"),
        // CAN/SUB abortam: o que vem depois e texto.
        (b"\x1b[5\x18f", b"\x1b[5\x18f"),
        (b"\x1b[5\x1af", b"\x1b[5\x1af"),
        (b"\x1b[\x18s", b"\x1b[\x18s"),
        (b"\x1b\x18[s", b"\x1b\x18[s"),
        // ESC aborta e recomeca.
        (b"\x1b[5\x1b[3;4f", b"\x1b[5\x1b[3;4H"),
        (b"\x1b[\x1b[s", b"\x1b[\x1b7"),
        (b"\x1b\x1b[s", b"\x1b\x1b7"),
        (b"\x1b[?\x1b[u", b"\x1b[?\x1b8"),
        // ESC com intermediario: o '[' nao abre CSI.
        (b"\x1b([s", b"\x1b([s"),
        (b"\x1b#8[f", b"\x1b#8[f"),
        // Strings: o conteudo nunca muda.
        (b"\x1b]0;[f[s\x07", b"\x1b]0;[f[s\x07"),
        (b"\x1b]2;a[5;5f\x1b\\", b"\x1b]2;a[5;5f\x1b\\"),
        (b"\x1bP1;2|[f[s\x1b\\", b"\x1bP1;2|[f[s\x1b\\"),
        (b"\x1b_[s\x1b\\", b"\x1b_[s\x1b\\"),
        (b"\x1b^[u\x1b\\", b"\x1b^[u\x1b\\"),
        (b"\x1bX[f\x1b\\", b"\x1bX[f\x1b\\"),
        (b"\x1bP[f\x9c[s", b"\x1bP[f\x9c[s"),
        // ...mas o ESC encerra a string, e o que vem depois e escape de verdade.
        (b"\x1b]0;t\x1b[s", b"\x1b]0;t\x1b7"),
        // C1 de 8 bits nao e CSI; UTF-8 passa intacto.
        (b"\x9b5;10f\x9bs", b"\x9b5;10f\x9bs"),
        ("ção ⣿ ─│".as_bytes(), "ção ⣿ ─│".as_bytes()),
        (b"\xc3\x1b[s", b"\xc3\x1b7"),
        (b"\xc3\xa7\x1b\x80[s", b"\xc3\xa7\x1b\x807"),
        // Limites.
        (b"", b""),
        (b"\x1b", b"\x1b"),
        (b"\x1b[", b"\x1b"),
        (b"[s[u[f", b"[s[u[f"),
    ];

    #[test]
    fn translates_only_what_vt100_dispatches() {
        for (input, want) in CASES {
            let got = fix(input);
            assert_eq!(
                got.out,
                *want,
                "entrada {:?}: saiu {:?}",
                String::from_utf8_lossy(input),
                String::from_utf8_lossy(&got.out)
            );
            // So "ESC [" no fim fica segurado.
            let held: &[u8] = if input.ends_with(b"\x1b[") { b"[" } else { b"" };
            assert_eq!(got.pending, held);
        }
    }

    #[test]
    fn held_bracket_survives_empty_chunks() {
        let mut f = VtFix::default();
        let mut sink = Sink::default();
        for chunk in [&b"a\x1b"[..], b"[", b"", b"", b"s", b"b"] {
            f.feed(chunk, |p| sink.take(p));
        }
        assert_eq!(sink.fixed.out, b"a\x1b7b");
        assert_eq!(f.pending(), b"");
        // Um '[' segurado que nao vira 7/8 volta antes do byte seguinte.
        let mut sink = Sink::default();
        for chunk in [&b"\x1b["[..], b"", b"2J"] {
            f.feed(chunk, |p| sink.take(p));
        }
        assert_eq!(sink.fixed.out, b"\x1b[2J");
    }

    #[test]
    fn same_output_for_every_cut() {
        for (input, _) in CASES {
            assert_cut_invariant(input, true);
        }
        // Uma tela inteira com varios alvos: todos os cortes simples.
        let mut frame = Vec::new();
        for i in 1..=30u32 {
            frame.extend_from_slice(format!("\x1b[{i};{}f", i * 2).as_bytes());
            frame.extend_from_slice("⣿─°C│\x1b[s\x1b]0;[f\x07\x1b[u\x1b[?25l".as_bytes());
        }
        frame.extend_from_slice(b"\x1b[?1049h\x1b[s\x1b[u\x1b[?1049l");
        assert_cut_invariant(&frame, false);
    }

    #[test]
    fn plain_csi_is_not_split() {
        // Cada trecho e uma chamada ao parser: os CSI sem troca seguem no
        // mesmo trecho do texto em volta.
        assert_eq!(
            items(&[b"a\x1b[31mb\x1b[0m\x1b[?25l\x1b[2J\x1b[1;1H\x1b[?1049"]),
            [b(b"a\x1b[31mb\x1b[0m\x1b[?25l\x1b[2J\x1b[1;1H\x1b[?1049")]
        );
        // So as trocas e os avisos cortam.
        assert_eq!(
            items(&[b"\x1b[sx\x1b[2;3fy\x1b[?1049hz\x1b[u"]),
            [
                b(b"\x1b"),
                b(b"7"),
                b(b"x\x1b[2;3"),
                b(b"H"),
                b(b"y\x1b[?1049"),
                b(b"h"),
                Item::Alt(true),
                b(b"z\x1b"),
                b(b"8"),
            ]
        );
    }

    #[test]
    fn split_utf8_char_goes_whole() {
        // O caractere incompleto do fim fica segurado e vai inteiro.
        assert_eq!(
            items(&[b"45\xc2", b"\xb0C\xe2\x94\x82"]),
            [b(b"45"), b("°".as_bytes()), b("C│".as_bytes())]
        );
        // De 4 bytes, em 4 pedacos (e pedacos vazios no meio).
        assert_eq!(
            items(&[b"a\xf0", b"", b"\x9f", b"\x98", b"\x80b"]),
            [b(b"a"), b("\u{1f600}".as_bytes()), b(b"b")]
        );
        // Interrompido por outro byte: o que veio vai sozinho (o vte o troca
        // por U+FFFD) e o resto segue normalmente.
        assert_eq!(items(&[b"\xe2\x94", b"x"]), [b(b"\xe2\x94"), b(b"x")]);
        assert_eq!(
            items(&[b"\xc3", b"\x1b[s"]),
            [b(b"\xc3"), b(b"\x1b"), b(b"7")]
        );
        // Invalido (E0 80): vai junto do que veio, sem segurar mais.
        assert_eq!(
            items(&[b"\xe0", b"\x80\x80z"]),
            [b(b"\xe0\x80"), b(b"\x80z")]
        );
        // O que fica segurado no fim: so o comeco valido de um caractere.
        let cases: &[(&[u8], &[u8])] = &[
            (b"a\xe2\x94", b"\xe2\x94"),
            (b"\xf0\x9f\x98", b"\xf0\x9f\x98"),
            (b"\xc3\xc3", b"\xc3"),
            ("é".as_bytes(), b""),
            (b"\x80", b""),
            (b"\xe0\x80", b""),
            (b"\xed\xa0", b""),
            (b"\xf5", b""),
            (b"a\x1b[?1049h\xc3", b"\xc3"),
            (b"\x1b]0;t\xc3", b"\xc3"),
            // Dentro do escape ou do CSI o vte ignora bytes altos: nada segurado.
            (b"\x1b\xc3", b""),
            (b"\x1b[\xc3", b""),
        ];
        for &(input, held) in cases {
            let got = fix(input);
            assert_eq!(got.pending, held, "{input:?}");
            assert_eq!([got.out, got.pending].concat(), input);
        }
    }

    /// Texto sem ESC (tudo no estado de texto do vte), cortado ao acaso: o
    /// vte nunca recebe o resto de um caractere num trecho diferente do
    /// comeco (e o que dispara a perda de bytes no `advance_partial_utf8`).
    #[test]
    fn utf8_never_reaches_vte_split() {
        const TEXT: &[&[u8]] = &[
            b"a",
            b"C",
            b" ",
            b"\xc2\xb0",
            b"\xc3\xa7",
            b"\xe2\x94\x82",
            b"\xe2\xa3\xbf",
            b"\xf0\x9f\x98\x80",
            b"\xc3",
            b"\xe2\x94",
            b"\xf0\x9f",
            b"\x80",
            b"\xbf",
            b"\xe0\x80",
            b"\xed\xa0\x80",
            b"\xf5",
            b"\xff",
            b"\x07",
            b"\r",
        ];
        let mut rng = Rng(0x0dd_ba11_c0ff_ee00);
        for _ in 0..5_000 {
            let s = rng.tokens(TEXT, 16);
            let mut cuts: Vec<usize> = (0..rng.below(5)).map(|_| rng.below(s.len() + 1)).collect();
            cuts.sort_unstable();
            let mut f = VtFix::default();
            let mut out = Vec::new();
            let mut from = 0;
            for &c in cuts.iter().chain(std::iter::once(&s.len())) {
                f.feed(&s[from..c], |p| {
                    let Piece::Bytes(p) = p else {
                        panic!("aviso sem CSI")
                    };
                    assert!(
                        utf8_tail(&out) == 0 || !is_cont(p[0]),
                        "caractere partido: {out:?} | {p:?}"
                    );
                    out.extend_from_slice(p);
                });
                from = c;
            }
            out.extend(f.pending());
            assert_eq!(out, s);
        }
    }

    #[test]
    fn alt_screen_notices() {
        let cases: &[(&[u8], &[Notice])] = &[
            (b"\x1b[?1049h", &[(8, true)]),
            (b"a\x1b[?1049lb", &[(9, false)]),
            (b"\x1b[?01049h", &[(9, true)]),
            (b"\x1b[?\r1049\x7fh", &[(10, true)]),
            (b"\x1b\r[?1049h", &[(9, true)]),
            (b"\x1b[?1049h\x1b[?1049l", &[(8, true), (16, false)]),
            (b"\x1b]0;\x1b[?1049h", &[(12, true)]),
            (b"\x1b[?1049\x1b[?1049h", &[(15, true)]),
            // Nada: outros modos, mais parametros, sem '?', intermediario,
            // marcador repetido, valor saturado, abortado, outro final, strings.
            (b"\x1b[?25h\x1b[?1049;25h\x1b[?25;1049l\x1b[?1049:1h", &[]),
            (b"\x1b[1049h\x1b[?1049$h\x1b[??1049h\x1b[>1049h", &[]),
            (b"\x1b[?104900h\x1b[?1049\x18h\x1b[?1049H\x1b[?1049s", &[]),
            (b"\x1b]0;[?1049h\x07\x1bP[?1049h\x1b\\\x1b([?1049h", &[]),
        ];
        for (input, want) in cases {
            let got = fix(input);
            assert_eq!(got.out, *input);
            assert_eq!(got.alt, *want, "{:?}", String::from_utf8_lossy(input));
            assert_cut_invariant(input, true);
        }
    }

    /// Gerador deterministico (xorshift64*), sem dependencias.
    pub(crate) struct Rng(pub(crate) u64);

    impl Rng {
        pub(crate) fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        pub(crate) fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        /// Fluxo aleatorio com os bytes que importam bem representados.
        fn stream(&mut self, alphabet: &[u8], max_len: usize) -> Vec<u8> {
            let len = self.below(max_len + 1);
            (0..len)
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }

        /// Fluxo aleatorio de ate `max` pedacos de `tokens`.
        pub(crate) fn tokens(&mut self, tokens: &[&[u8]], max: usize) -> Vec<u8> {
            let n = self.below(max + 1);
            let mut v = Vec::new();
            for _ in 0..n {
                v.extend_from_slice(tokens[self.below(tokens.len())]);
            }
            v
        }
    }

    const ALPHABET: &[u8] = b"\x1b\x1b\x1b\x1b\x1b\x1b[[[[[[]]]P^_X\\01259;;:?><= !#(fffsssuuuHAmah\x07\r\n\x08\x05\x0e\x19\x1c\x18\x1a\x7f\x9c\x9b\xc3\xa9\xe2\xa3\xbf\x80\xff\x00";

    /// Pedacos para montar sequencias inteiras (1049, strings, UTF-8).
    const TOKENS: &[&[u8]] = &[
        b"\x1b",
        b"\x1b[",
        b"[",
        b"?",
        b"1049",
        b"104",
        b"9",
        b"0",
        b"1",
        b";",
        b":",
        b"$",
        b" ",
        b"h",
        b"l",
        b"H",
        b"f",
        b"s",
        b"u",
        b"m",
        b"\r",
        b"\x07",
        b"\x18",
        b"\x7f",
        b"]",
        b"P",
        b"\\",
        b"\x9c",
        b"\x80",
        b"\xc3",
        b"\xa7",
        b"\xe2\xa3",
        b"\xbf",
        b"a",
    ];

    /// Referencia independente: a maquina de estados do vte 0.15 inteira
    /// (todos os estados, como em vte/src/lib.rs), aplicada ao fluxo todo de
    /// uma vez, trocando o byte final dos CSI que o vt100 despacharia. Devolve
    /// tambem os avisos (bytes ate logo depois do final, entrou?) dos
    /// `CSI ? 1049 h`/`l` com esse unico parametro.
    fn reference(input: &[u8]) -> (Vec<u8>, Vec<Notice>) {
        let (out, alt, _) = reference_full(input);
        (out, alt)
    }

    /// A referencia, dizendo tambem se o vte termina no estado base (fora
    /// de escape, CSI e strings), que e o que `idle` promete. Um DCS que o
    /// vte encerrou pelo 0x9C conta como pendente ate o proximo ESC/CAN/SUB
    /// (o tradutor nao acompanha os estados do DCS: fica do lado seguro).
    fn reference_full(input: &[u8]) -> (Vec<u8>, Vec<Notice>, bool) {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum St {
            Ground,
            Esc,
            EscInt,
            CsiEntry,
            CsiParam,
            CsiInt,
            CsiIgnore,
            DcsEntry,
            DcsParam,
            DcsInt,
            DcsIgnore,
            DcsPass,
            Osc,
            SosPmApc,
        }
        use St::*;
        let exec = |b: u8| matches!(b, 0x00..=0x17 | 0x19 | 0x1c..=0x1f);
        // "anywhere" do vte: CAN/SUB voltam ao texto, ESC recomeca.
        let anywhere = |b: u8, st: St| match b {
            0x18 | 0x1a => Ground,
            0x1b => Esc,
            _ => st,
        };
        let mut st = Ground;
        let mut inter = false; // intermediario ou marcador privado coletado
        let mut param = false; // algum byte de parametro
        let mut dec: Option<u16> = None; // so '?' e digitos ate aqui: o valor
        let mut bracket = 0; // posicao em `out` do '[' que abriu o CSI
        let mut out = Vec::new();
        let mut alt = Vec::new();
        let mut dcs_c1_end = false; // DCS encerrado pelo 0x9C (ver acima)
        for &b in input {
            let mut byte = Some(b);
            let mut notice = None;
            st = match st {
                Ground => {
                    if matches!(b, 0x18 | 0x1a | 0x1b) {
                        dcs_c1_end = false;
                    }
                    if b == 0x1b {
                        Esc
                    } else {
                        Ground
                    }
                }
                Esc => match b {
                    0x1b => Esc,
                    _ if exec(b) => Esc,
                    0x18 | 0x1a => Ground,
                    0x20..=0x2f => EscInt,
                    b'P' => DcsEntry,
                    b'X' | b'^' | b'_' => SosPmApc,
                    b'[' => {
                        // reset_params do vte ao entrar no CSI.
                        bracket = out.len();
                        inter = false;
                        param = false;
                        dec = None;
                        CsiEntry
                    }
                    b']' => Osc,
                    0x30..=0x7e => Ground,
                    _ => Esc,
                },
                EscInt => match b {
                    _ if exec(b) => EscInt,
                    0x20..=0x2f | 0x7f => EscInt,
                    0x30..=0x7e => Ground,
                    _ => anywhere(b, EscInt),
                },
                CsiEntry | CsiParam | CsiInt => {
                    let s = st;
                    match b {
                        _ if exec(b) => s,
                        0x20..=0x2f => {
                            inter = true;
                            dec = None;
                            CsiInt
                        }
                        0x30..=0x3b if s == CsiInt => CsiIgnore,
                        0x30..=0x3b => {
                            param = true;
                            dec = match b {
                                b'0'..=b'9' => dec.map(|n| {
                                    n.saturating_mul(10).saturating_add(u16::from(b - b'0'))
                                }),
                                _ => None,
                            };
                            CsiParam
                        }
                        0x3c..=0x3f if s == CsiEntry => {
                            inter = true;
                            dec = (b == b'?').then_some(0);
                            CsiParam
                        }
                        0x3c..=0x3f => CsiIgnore,
                        0x40..=0x7e => {
                            // csi_dispatch; o vt100 so olha o 1o intermediario.
                            if !inter {
                                match b {
                                    b'f' => byte = Some(b'H'),
                                    b's' | b'u' if !param => {
                                        if out.len() == bracket + 1 {
                                            // Colado no "ESC [": tira o '['.
                                            out.pop();
                                        } else {
                                            out.push(0x1b);
                                        }
                                        byte = Some(if b == b's' { b'7' } else { b'8' });
                                    }
                                    _ => {}
                                }
                            } else if dec == Some(1049) && matches!(b, b'h' | b'l') {
                                notice = Some(b == b'h');
                            }
                            Ground
                        }
                        0x7f => s,
                        _ => anywhere(b, s),
                    }
                }
                CsiIgnore => match b {
                    _ if exec(b) => CsiIgnore,
                    0x20..=0x3f | 0x7f => CsiIgnore,
                    0x40..=0x7e => Ground,
                    _ => anywhere(b, CsiIgnore),
                },
                DcsEntry | DcsParam | DcsInt => {
                    let s = st;
                    match b {
                        _ if exec(b) => s,
                        0x20..=0x2f => DcsInt,
                        0x30..=0x3b if s == DcsInt => DcsIgnore,
                        0x30..=0x3b => DcsParam,
                        0x3c..=0x3f if s == DcsEntry => DcsParam,
                        0x3c..=0x3f => DcsIgnore,
                        0x40..=0x7e => DcsPass,
                        0x7f => s,
                        _ => anywhere(b, s),
                    }
                }
                DcsIgnore => anywhere(b, DcsIgnore),
                DcsPass => match b {
                    0x9c => {
                        dcs_c1_end = true;
                        Ground
                    }
                    _ => anywhere(b, DcsPass),
                },
                Osc => match b {
                    0x07 => Ground,
                    _ => anywhere(b, Osc),
                },
                SosPmApc => anywhere(b, SosPmApc),
            };
            out.extend(byte);
            if let Some(on) = notice {
                alt.push((out.len(), on));
            }
        }
        (out, alt, st == Ground && !dcs_c1_end)
    }

    /// `idle` so e verdadeiro com o vte no estado base e nada segurado, em
    /// qualquer ponto do fluxo (o `Terminal` injeta bytes proprios ai: com
    /// o vte no meio de um escape ou string, o resto dele iria para a tela).
    #[test]
    fn idle_only_when_vte_is_in_ground() {
        let check = |s: &[u8]| {
            let mut f = VtFix::default();
            for c in 0..=s.len() {
                if c > 0 {
                    f.feed(&s[c - 1..c], |_| {});
                }
                let (_, _, ground) = reference_full(&s[..c]);
                assert_eq!(
                    f.idle(),
                    ground && f.pending().is_empty(),
                    "depois de {:?}",
                    &s[..c]
                );
            }
            f.idle()
        };
        // sgr0 do ncurses ("ESC ( B ESC [ m"), titulo do PROMPT_COMMAND do
        // AlmaLinux, DCS, APC, DECALN e intermediarios seguidos.
        for (s, idle) in [
            (&b"$ \x1b("[..], false),
            (b"$ \x1b(B", true),
            (b"$ \x1b(B\x1b[m", true),
            (b"\x1b(\r\x7f\xc3", false),
            (b"\x1b ([", true),
            (b"\x1b#", false),
            (b"\x1b#8", true),
            (b"\x1b]0;root@srv01:~", false),
            (b"\x1b]0;root@srv01:~\x07", true),
            (b"\x1b]0;t\x1b", false),
            (b"\x1b]0;t\x1b\\", true),
            (b"\x1b]0;t\x18", true),
            (b"\x1bP1$r0m", false),
            (b"\x1bP1$r0m\x1b\\", true),
            (b"\x1b_x\x07", false),
            (b"\x1b_x\x1a", true),
            (b"\x1b[3", false),
            (b"\x1b[", false),
            (b"\xc3", false),
            (b"a\xc3\xa7", true),
        ] {
            assert_eq!(check(s), idle, "{s:?}");
        }
        let mut rng = Rng(0x1d1e_5eed_0000_0001);
        for _ in 0..5_000 {
            check(&rng.stream(ALPHABET, 40));
        }
        for _ in 0..5_000 {
            check(&rng.tokens(TOKENS, 24));
        }
    }

    #[test]
    fn matches_full_vte_state_machine() {
        let check = |s: &[u8]| {
            let got = fix(s);
            // A referencia repassa na hora o que o tradutor segura no fim.
            let out = [got.out, got.pending].concat();
            assert_eq!((out, got.alt), reference(s), "entrada {s:?}");
        };
        let mut rng = Rng(0x5a60_7e2d_1234_5678);
        for _ in 0..20_000 {
            check(&rng.stream(ALPHABET, 40));
        }
        for _ in 0..20_000 {
            check(&rng.tokens(TOKENS, 24));
        }
        // A referencia concorda com os casos escritos a mao.
        for (input, want) in CASES {
            let got = fix(input);
            let out = [got.out, got.pending].concat();
            assert_eq!(
                reference(input),
                (out, got.alt),
                "{input:?} (esperado {want:?})"
            );
        }
    }

    #[test]
    fn random_streams_cut_anywhere() {
        let mut rng = Rng(0x0123_4567_89ab_cdef);
        for _ in 0..3_000 {
            let s = rng.stream(ALPHABET, 48);
            assert_cut_invariant(&s, false);
        }
        for _ in 0..1_500 {
            let s = rng.tokens(TOKENS, 20);
            assert_cut_invariant(&s, false);
        }
    }

    #[test]
    fn identity_without_f_s_u() {
        let alphabet: Vec<u8> = ALPHABET
            .iter()
            .copied()
            .filter(|b| !b"fsu".contains(b))
            .collect();
        let mut rng = Rng(0xfeed_beef_cafe_f00d);
        for _ in 0..20_000 {
            let s = rng.stream(&alphabet, 64);
            let got = fix(&s);
            assert_eq!([got.out, got.pending].concat(), s);
        }
        // Todos os bytes, soltos e depois de "ESC [" (exceto os tres finais).
        let all: Vec<u8> = (0..=255u8).filter(|b| !b"fsu".contains(b)).collect();
        assert_eq!(fix(&all).out, all);
        for b in all {
            let s = [0x1b, b'[', b, b'1', b'H'];
            let got = fix(&s);
            assert_eq!(
                (got.out.as_slice(), got.pending.as_slice()),
                (&s[..], b"" as &[u8]),
                "byte {b:#04x}"
            );
        }
    }
}
