//! Emojis coloridos (e simbolos que faltam na fonte) no terminal.
//!
//! O egui so desenha glifos monocromaticos (mascara alfa num atlas) e a fonte
//! embutida nao tem emojis novos (🟢, 🟠...) nem milhares de simbolos (✓, 🛢,
//! CJK...): sairiam como um quadrado vazio. No Windows o DirectWrite (o mesmo
//! que desenha o seletor Win+.) rasteriza o texto da "Segoe UI Emoji", com as
//! cores, e cai sozinho na fonte do sistema que tiver o caractere; cada celula
//! vira uma imagem RGBA guardada como textura.
//!
//! Fora do Windows (ou se o DirectWrite falhar) nada aqui desenha e o
//! terminal fica com o glifo da fonte, como antes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Color32, ColorImage, Context, TextureHandle, TextureId, TextureOptions};

/// A celula do vt100 (`s` = conteudo, `wide` = ocupa duas colunas) e um emoji
/// para desenhar colorido?
///
/// Vale o que o Unicode manda desenhar como emoji: caractere com apresentacao
/// de emoji por padrao (ocupa duas colunas) ou pictografico seguido do seletor
/// de variacao U+FE0F. Simbolos de texto comuns (✓ ★ ▶ ●), que o htop e o btop
/// usam, continuam com a fonte do terminal.
pub fn is_emoji(s: &str, wide: bool) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if chars.any(|c| c == '\u{fe0f}') {
        return is_pictographic(first);
    }
    wide && has_emoji_presentation(first)
}

/// Emoji por padrao (Emoji_Presentation) entre os que o vt100 mede com duas
/// colunas: todo o plano 1 de pictogramas e os poucos da faixa BMP.
fn has_emoji_presentation(c: char) -> bool {
    let u = u32::from(c);
    if u >= 0x1f000 {
        return u <= 0x1faff;
    }
    matches!(
        u,
        0x231a | 0x231b
            | 0x23e9..=0x23ec
            | 0x23f0
            | 0x23f3
            | 0x25fd | 0x25fe
            | 0x2614 | 0x2615
            | 0x2648..=0x2653
            | 0x267f
            | 0x2693
            | 0x26a1
            | 0x26aa | 0x26ab
            | 0x26bd | 0x26be
            | 0x26c4 | 0x26c5
            | 0x26ce
            | 0x26d4
            | 0x26ea
            | 0x26f2 | 0x26f3
            | 0x26f5
            | 0x26fa
            | 0x26fd
            | 0x2705
            | 0x270a | 0x270b
            | 0x2728
            | 0x274c
            | 0x274e
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27b0
            | 0x27bf
            | 0x2b1b | 0x2b1c
            | 0x2b50
            | 0x2b55
    )
}

/// Pictografico (Extended_Pictographic, emoji-data do Unicode): o que o
/// U+FE0F transforma em emoji e o que, faltando na fonte, sai colorido.
pub fn is_pictographic(c: char) -> bool {
    matches!(
        u32::from(c),
        0xa9 | 0xae
            | 0x203c
            | 0x2049
            | 0x2122
            | 0x2139
            | 0x2194..=0x2199
            | 0x21a9..=0x21aa
            | 0x231a..=0x231b
            | 0x2328
            | 0x2388
            | 0x23cf
            | 0x23e9..=0x23f3
            | 0x23f8..=0x23fa
            | 0x24c2
            | 0x25aa..=0x25ab
            | 0x25b6
            | 0x25c0
            | 0x25fb..=0x25fe
            | 0x2600..=0x2605
            | 0x2607..=0x2612
            | 0x2614..=0x2685
            | 0x2690..=0x2705
            | 0x2708..=0x2712
            | 0x2714
            | 0x2716
            | 0x271d
            | 0x2721
            | 0x2728
            | 0x2733..=0x2734
            | 0x2744
            | 0x2747
            | 0x274c
            | 0x274e
            | 0x2753..=0x2755
            | 0x2757
            | 0x2763..=0x2767
            | 0x2795..=0x2797
            | 0x27a1
            | 0x27b0
            | 0x27bf
            | 0x2934..=0x2935
            | 0x2b05..=0x2b07
            | 0x2b1b..=0x2b1c
            | 0x2b50
            | 0x2b55
            | 0x3030
            | 0x303d
            | 0x3297
            | 0x3299
            | 0x1f000..=0x1f0ff
            | 0x1f10d..=0x1f10f
            | 0x1f12f
            | 0x1f16c..=0x1f171
            | 0x1f17e..=0x1f17f
            | 0x1f18e
            | 0x1f191..=0x1f19a
            | 0x1f1ad..=0x1f1e5
            | 0x1f201..=0x1f20f
            | 0x1f21a
            | 0x1f22f
            | 0x1f232..=0x1f23a
            | 0x1f23c..=0x1f23f
            | 0x1f249..=0x1f3fa
            | 0x1f400..=0x1f53d
            | 0x1f546..=0x1f64f
            | 0x1f680..=0x1f6ff
            | 0x1f774..=0x1f77f
            | 0x1f7d5..=0x1f7ff
            | 0x1f80c..=0x1f80f
            | 0x1f848..=0x1f84f
            | 0x1f85a..=0x1f85f
            | 0x1f888..=0x1f88f
            | 0x1f8ae..=0x1f8ff
            | 0x1f90c..=0x1f93a
            | 0x1f93c..=0x1f945
            | 0x1f947..=0x1faff
            | 0x1fc00..=0x1fffd
    )
}

/// Tamanho da fonte (pixels) para um emoji caber numa caixa `w` x `h`.
pub fn emoji_font_px(w: u32, h: u32) -> f32 {
    w.min(h) as f32 * 0.9
}

/// Folga transparente (pixels) que a imagem leva em cada lado da caixa
/// `w` x `h`: a sombra e as bordas dos emojis passam um pouco da caixa da
/// fonte e seriam cortadas na borda da celula.
pub fn pad_px(w: u32, h: u32) -> u32 {
    w.min(h).div_ceil(4)
}

/// Imagem RGBA (alfa pre-multiplicado) de `text` com fonte de `font_px`
/// pixels, centralizado numa caixa de `w` x `h` pixels, com [`pad_px`] de
/// folga em volta (a imagem mede `w + 2p` x `h + 2p`). Emojis saem com as
/// proprias cores; o que nao tem cor (simbolos de outras fontes), em `fg`.
/// `None` se o sistema nao desenhar.
pub fn render(text: &str, w: u32, h: u32, font_px: f32, fg: Color32) -> Option<ColorImage> {
    #[cfg(windows)]
    {
        win::render(text, w, h, font_px, fg)
    }
    #[cfg(not(windows))]
    {
        let _ = (text, w, h, font_px, fg);
        None
    }
}

/// Texturas ja rasterizadas, por (texto, caixa, fonte, cor). `None`: o
/// sistema nao soube desenhar (nao tenta de novo a cada quadro).
type Cache = HashMap<(String, u32, u32, u32, Color32), Option<TextureHandle>>;

/// Tanto quanto uma tela cheia de emojis diferentes: passou disso, recomeca.
const CACHE_MAX: usize = 512;

/// Textura de [`render`] (criada na primeira vez e guardada no contexto):
/// pinte-a na caixa `w` x `h` aumentada em `pad_px` de cada lado. `None` se
/// nao der para desenhar pelo sistema: quem chama usa a fonte.
pub fn texture(
    ctx: &Context,
    text: &str,
    w: u32,
    h: u32,
    font_px: f32,
    fg: Color32,
) -> Option<TextureId> {
    let cache = ctx.data_mut(|d| {
        d.get_temp_mut_or_insert_with::<Arc<Mutex<Cache>>>(egui::Id::new("sagu_emoji"), || {
            Arc::new(Mutex::new(Cache::new()))
        })
        .clone()
    });
    let mut cache = cache.lock().ok()?;
    if cache.len() >= CACHE_MAX {
        cache.clear();
    }
    let key = (text.to_owned(), w, h, font_px.to_bits(), fg);
    if !cache.contains_key(&key) {
        let tex = render(text, w, h, font_px, fg)
            .map(|img| ctx.load_texture(format!("emoji:{text}"), img, TextureOptions::LINEAR));
        cache.insert(key.clone(), tex);
    }
    cache.get(&key)?.as_ref().map(|t| t.id())
}

#[cfg(windows)]
mod win {
    use std::cell::Cell;

    use egui::{Color32, ColorImage};
    use windows::core::{w, Result};
    use windows::Win32::Graphics::Direct2D::Common::{
        D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F,
    };
    use windows::Win32::Graphics::Direct2D::{
        D2D1CreateFactory, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
        D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
        D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
    };
    use windows::Win32::Graphics::DirectWrite::{
        DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED,
        DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
        DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
        DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_WORD_WRAPPING_NO_WRAP,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
        WICBitmapCacheOnDemand, WICBitmapLockRead, WICRect,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };

    /// Os objetos do DirectWrite/Direct2D/WIC, criados uma vez por thread (a
    /// da interface, a unica que desenha).
    struct Renderer {
        d2d: ID2D1Factory,
        dwrite: IDWriteFactory,
        wic: IWICImagingFactory,
    }

    thread_local! {
        /// `Some(None)`: ja tentou e falhou (nao adianta de novo a cada
        /// quadro). Sem destrutor de proposito: o renderizador nunca e solto.
        static RENDERER: Cell<Option<Option<&'static Renderer>>> = const { Cell::new(None) };
    }

    impl Renderer {
        fn new() -> Result<Self> {
            unsafe {
                // O winit ja inicia o COM na thread da interface; o resultado
                // (S_FALSE ou modo diferente) nao importa.
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                Ok(Self {
                    d2d: D2D1CreateFactory::<ID2D1Factory>(
                        D2D1_FACTORY_TYPE_SINGLE_THREADED,
                        None,
                    )?,
                    dwrite: DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED)?,
                    wic: CoCreateInstance::<_, IWICImagingFactory>(
                        &CLSID_WICImagingFactory,
                        None,
                        CLSCTX_INPROC_SERVER,
                    )?,
                })
            }
        }

        fn render(&self, text: &str, w: u32, h: u32, fonte: f32, fg: Color32) -> Result<ColorImage> {
            let wide: Vec<u16> = text.encode_utf16().collect();
            let pad = super::pad_px(w, h);
            let (cw, ch) = (w + 2 * pad, h + 2 * pad);
            // Desenha `k` vezes maior e reduz pela media: no tamanho da celula
            // (14 px) o hinting da fonte encaixa as curvas na grade de pixels
            // e achata o alto e o baixo dos circulos (🟢 parecia cortado).
            let k = (256 / cw.max(ch)).clamp(1, 4);
            let (bw, bh) = (cw * k, ch * k);
            let kf = k as f32;
            // A tinta do emoji fica cerca de 5% do tamanho da fonte abaixo do
            // centro da linha de texto (medido): sobe-se o texto para
            // centralizar, em pixels da imagem grande (fracao de pixel na
            // final, para os circulos ficarem simetricos).
            let subida = (fonte * kf * 0.05).round().max(1.0);
            unsafe {
                let bitmap = self.wic.CreateBitmap(
                    bw,
                    bh,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnDemand,
                )?;
                let props = D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    // 96 dpi: uma unidade do Direct2D e um pixel.
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    ..Default::default()
                };
                let target = self.d2d.CreateWicBitmapRenderTarget(&bitmap, &props)?;
                // Cor do que nao e emoji colorido (e da "cor do texto" que
                // alguns emojis usam).
                let brush = target.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: f32::from(fg.r()) / 255.0,
                        g: f32::from(fg.g()) / 255.0,
                        b: f32::from(fg.b()) / 255.0,
                        a: 1.0,
                    },
                    None,
                )?;

                let format = self.dwrite.CreateTextFormat(
                    w!("Segoe UI Emoji"),
                    None,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    fonte * kf,
                    w!("en-us"),
                )?;
                format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
                format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;

                target.BeginDraw();
                target.Clear(Some(&D2D1_COLOR_F::default()));
                target.DrawText(
                    &wide,
                    &format,
                    &D2D_RECT_F {
                        left: pad as f32 * kf,
                        top: pad as f32 * kf - subida,
                        right: (pad + w) as f32 * kf,
                        bottom: (pad + h) as f32 * kf - subida,
                    },
                    &brush,
                    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    DWRITE_MEASURING_MODE_NATURAL,
                );
                target.EndDraw(None, None)?;

                let lock = bitmap.Lock(
                    &WICRect {
                        X: 0,
                        Y: 0,
                        Width: bw as i32,
                        Height: bh as i32,
                    },
                    WICBitmapLockRead.0 as u32,
                )?;
                let stride = lock.GetStride()? as usize;
                let mut len = 0u32;
                let mut ptr = std::ptr::null_mut();
                lock.GetDataPointer(&mut len, &mut ptr)?;
                let data = std::slice::from_raw_parts(ptr, len as usize);

                // Media de cada bloco k x k (a media de cores pre-multiplicadas
                // e a reducao certa), e BGRA -> RGBA.
                let k = k as usize;
                let n = (k * k) as u32;
                let mut rgba = Vec::with_capacity(cw as usize * ch as usize * 4);
                for y in 0..ch as usize {
                    for x in 0..cw as usize {
                        let mut soma = [0u32; 4];
                        for dy in 0..k {
                            let linha = (y * k + dy) * stride + x * k * 4;
                            for px in data[linha..linha + k * 4].chunks_exact(4) {
                                soma[0] += u32::from(px[2]);
                                soma[1] += u32::from(px[1]);
                                soma[2] += u32::from(px[0]);
                                soma[3] += u32::from(px[3]);
                            }
                        }
                        rgba.extend(soma.map(|v| ((v + n / 2) / n) as u8));
                    }
                }
                Ok(ColorImage::from_rgba_premultiplied(
                    [cw as usize, ch as usize],
                    &rgba,
                ))
            }
        }
    }

    pub(super) fn render(
        text: &str,
        w: u32,
        h: u32,
        fonte: f32,
        fg: Color32,
    ) -> Option<ColorImage> {
        if w == 0 || h == 0 || fonte <= 0.0 {
            return None;
        }
        let renderer = RENDERER.with(|slot| {
            if slot.get().is_none() {
                // Vazado: soltar objetos do Direct2D no fim da thread (quando
                // o Rust roda os destrutores do thread_local, sob o bloqueio
                // do carregador de DLLs) trava a proxima thread que carregue
                // uma DLL. O processo guarda um conjunto por thread que
                // desenha (a da interface) ate encerrar.
                let novo = Renderer::new().ok().map(|r| &*Box::leak(Box::new(r)));
                slot.set(Some(novo));
            }
            slot.get().flatten()
        })?;
        renderer.render(text, w, h, fonte, fg).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_emoji_cells() {
        // Emoji por padrao (duas colunas): sao os que a fonte nao tem.
        for s in ["🟢", "🟠", "😀", "🚀", "✅", "⭐", "⚡"] {
            assert!(is_emoji(s, true), "{s}");
        }
        // Pictografico de texto + U+FE0F vira emoji, mesmo numa coluna so.
        for s in ["❤\u{fe0f}", "⚠\u{fe0f}", "✔\u{fe0f}"] {
            assert!(is_emoji(s, false), "{s}");
        }
        // O que o htop/btop desenham e texto comum: continua com a fonte.
        for s in ["", "a", "─", "│", "█", "▲", "▶", "●", "✓", "★", "⣿", "é", "°"] {
            assert!(!is_emoji(s, false), "{s:?}");
        }
        // Largos que nao sao emoji (CJK) tambem.
        for s in ["日", "한", "Ａ"] {
            assert!(!is_emoji(s, true), "{s}");
        }
    }

    /// O vt100 mede 🟢/🟠 com duas colunas e o conteudo da celula e o emoji
    /// inteiro: a grade reserva o espaco que a imagem vai ocupar.
    #[test]
    fn vt100_gives_new_emoji_two_columns() {
        let mut p = vt100::Parser::new(2, 10, 0);
        p.process("🟢🟠x".as_bytes());
        let s = p.screen();
        assert!(s.cell(0, 0).unwrap().is_wide());
        assert_eq!(s.cell(0, 0).unwrap().contents(), "🟢");
        assert!(s.cell(0, 1).unwrap().is_wide_continuation());
        assert_eq!(s.cell(0, 2).unwrap().contents(), "🟠");
        assert_eq!(s.cell(0, 4).unwrap().contents(), "x");
    }

    #[cfg(windows)]
    #[test]
    fn renders_colored_emoji() {
        for (s, w, h) in [("🟢", 17, 17), ("🟠", 34, 34), ("😀", 17, 17), ("❤\u{fe0f}", 8, 17)] {
            let img = render(s, w, h, emoji_font_px(w, h), Color32::WHITE).unwrap_or_else(|| panic!("{s}"));
            let p = pad_px(w, h);
            assert_eq!(img.size, [(w + 2 * p) as usize, (h + 2 * p) as usize]);
            let opacos: Vec<_> = img.pixels.iter().filter(|p| p.a() > 200).collect();
            assert!(opacos.len() > (w * h / 8) as usize, "{s}: {} opacos", opacos.len());
            // Colorido: nao e a silhueta branca do pincel do Direct2D.
            let [r, g, b, _] = opacos[opacos.len() / 2].to_array();
            assert!(!(r == g && g == b), "{s}: pixel cinza {r},{g},{b}");
        }
        // Verde e laranja se distinguem (cada um com a sua cor).
        let media = |s: &str| {
            let img = render(s, 34, 34, emoji_font_px(34, 34), Color32::WHITE).unwrap();
            let (mut r, mut g) = (0u32, 0u32);
            for p in img.pixels.iter().filter(|p| p.a() > 200) {
                r += u32::from(p.r());
                g += u32::from(p.g());
            }
            (r, g)
        };
        let (vr, vg) = media("🟢");
        let (lr, lg) = media("🟠");
        assert!(vg > vr, "verde: r={vr} g={vg}");
        assert!(lr > lg, "laranja: r={lr} g={lg}");
    }

    #[cfg(windows)]
    #[test]
    fn caches_the_texture_per_size() {
        let ctx = Context::default();
        let a = texture(&ctx, "🟢", 17, 17, emoji_font_px(17, 17), Color32::WHITE).expect("textura");
        assert_eq!(texture(&ctx, "🟢", 17, 17, emoji_font_px(17, 17), Color32::WHITE), Some(a));
        assert_ne!(texture(&ctx, "🟢", 34, 34, emoji_font_px(34, 34), Color32::WHITE), Some(a));
        assert_ne!(texture(&ctx, "🟠", 17, 17, emoji_font_px(17, 17), Color32::WHITE), Some(a));
        // A cor do texto entra na chave (simbolos sem cor saem nela).
        assert_ne!(texture(&ctx, "🟢", 17, 17, emoji_font_px(17, 17), Color32::RED), Some(a));
    }

    #[test]
    fn pictographs_follow_unicode() {
        // Pictogramas (com ou sem cor por padrao).
        for c in ['🛢', '🛠', '🖥', '⚙', '⚠', '✔', '❤', '▶', '★', '🟢', '🔹', '™', '©'] {
            assert!(is_pictographic(c), "{c}");
        }
        // Simbolos de texto, setas, caixas, braille, tom de pele, bandeira.
        for c in ['✓', '✗', '◆', '●', '▲', '─', '⣿', '⍺', '⏻', '🏽', '🇧', '日'] {
            assert!(!is_pictographic(c), "{c}");
        }
    }

    /// O que falta na fonte do terminal e nao e emoji (✓, CJK) sai de outra
    /// fonte do sistema, na cor do texto.
    #[cfg(windows)]
    #[test]
    fn renders_missing_symbols_in_text_color() {
        let verde = Color32::from_rgb(0x20, 0xd0, 0x40);
        for (s, w) in [("✓", 9), ("✗", 9), ("⍺", 9), ("日", 18)] {
            let img = render(s, w, 18, 15.0, verde).unwrap_or_else(|| panic!("{s}"));
            let tinta: Vec<_> = img.pixels.iter().filter(|p| p.a() > 160).collect();
            assert!(tinta.len() >= 4, "{s}: {} pixels de tinta", tinta.len());
            for p in tinta {
                // Pre-multiplicado: proporcional ao verde do texto.
                let [r, g, b, _] = p.to_array().map(u32::from);
                assert!(g > r * 3 && g > b * 2, "{s}: pixel {r},{g},{b} fora da cor do texto");
            }
        }
    }

    /// Retangulo (x0, y0, x1, y1) da tinta do emoji, em pixels da imagem.
    #[cfg(windows)]
    fn ink(img: &ColorImage) -> (i32, i32, i32, i32) {
        let cw = img.size[0] as i32;
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, -1, -1);
        for (i, px) in img.pixels.iter().enumerate() {
            if px.a() > 8 {
                let (x, y) = (i as i32 % cw, i as i32 / cw);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
        (x0, y0, x1, y1)
    }

    /// Circulos continuam redondos no tamanho da celula: a linha solida de
    /// cima e a de baixo sao curtas (no render direto, o hinting da fonte as
    /// alargava, achatando o 🟢 como se estivesse cortado).
    #[cfg(windows)]
    #[test]
    fn small_circles_stay_round() {
        for (w, h) in [(16, 17), (17, 17), (21, 21), (25, 25)] {
            for s in ["🟢", "🟠"] {
                let img = render(s, w, h, emoji_font_px(w, h), Color32::WHITE).unwrap();
                let cw = img.size[0];
                let linhas: Vec<usize> = img
                    .pixels
                    .chunks(cw)
                    .map(|l| l.iter().filter(|p| p.a() > 128).count())
                    .filter(|&n| n > 0)
                    .collect();
                let largura = *linhas.iter().max().unwrap();
                let (cima, baixo) = (linhas[0], linhas[linhas.len() - 1]);
                assert!(
                    cima * 2 <= largura && baixo * 2 <= largura,
                    "{s} {w}x{h}: borda achatada (cima {cima}, baixo {baixo}, largura {largura})"
                );
            }
        }
    }

    /// A tinta (sombra inclusa) nunca encosta na borda da imagem, que tem
    /// folga em volta da celula: nada e cortado. E o desenho fica centrado na
    /// celula, sem passar da base (onde o corte aparecia).
    #[cfg(windows)]
    #[test]
    fn emoji_is_never_clipped_and_is_centered() {
        for (w, h) in [(13, 26), (17, 17), (21, 21), (25, 25), (25, 31), (33, 33), (42, 42)] {
            let p = pad_px(w, h) as i32;
            for s in ["🟢", "😁", "🚀", "🌐", "✅", "⚠\u{fe0f}", "❤\u{fe0f}"] {
                let img = render(s, w, h, emoji_font_px(w, h), Color32::WHITE).unwrap();
                assert_eq!(img.size, [(w + 2 * p as u32) as usize, (h + 2 * p as u32) as usize]);
                let (x0, y0, x1, y1) = ink(&img);
                let (cw, ch) = (img.size[0] as i32, img.size[1] as i32);
                assert!(
                    x0 > 0 && y0 > 0 && x1 < cw - 1 && y1 < ch - 1,
                    "{s} {w}x{h}: tinta {x0},{y0}..{x1},{y1} encosta na borda ({cw}x{ch})"
                );
                // Dentro da celula (so 1 px de sombra de tolerancia).
                assert!(
                    x0 >= p - 1 && y0 >= p - 1 && x1 <= p + w as i32 && y1 <= p + h as i32,
                    "{s} {w}x{h}: tinta {},{}..{},{} passa da celula",
                    x0 - p, y0 - p, x1 - p, y1 - p
                );
                // Centrado na vertical, nos redondos que ocupam a caixa toda
                // (os de forma propria, como o triangulo do aviso, nao sao
                // simetricos; o rosto tem sombra embaixo: ate 2 px de folga).
                if matches!(s, "🟢" | "😁") {
                    let (topo, base) = (y0 - p, (h as i32 - 1) - (y1 - p));
                    assert!(
                        (topo - base).abs() <= 2,
                        "{s} {w}x{h}: margem de cima {topo}, de baixo {base}"
                    );
                }
            }
        }
    }
}



