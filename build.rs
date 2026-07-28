//! Build script: gera um `.ico` a partir de `assets/logo_mini.png` e o embute
//! como icone do executavel Windows (o que aparece no Explorer/barra de
//! tarefas), junto com os metadados de versao do arquivo.

use std::path::PathBuf;

use image::codecs::ico::{IcoEncoder, IcoFrame};
use image::imageops::FilterType;
use image::{ExtendedColorType, GenericImage, RgbaImage};

fn main() {
    // Icone de .exe e um recurso PE: so faz sentido no alvo Windows.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/logo_mini.png");

    let ico = build_ico();

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().expect("caminho do .ico"));
    res.set("ProductName", "SaguTerm");
    res.set("FileDescription", "SaguTerm — cliente SSH/SFTP");
    res.compile().expect("falha ao compilar o recurso do Windows");
}

/// Converte o logo em um .ico multi-tamanho (16..128, sem upscale) gravado em
/// OUT_DIR. O logo e centralizado num quadrado transparente caso nao seja
/// quadrado.
fn build_ico() -> PathBuf {
    let png = image::open("assets/logo_mini.png")
        .expect("assets/logo_mini.png")
        .into_rgba8();

    // Quadra a imagem com transparencia (no-op se ja for quadrada).
    let side = png.width().max(png.height());
    let mut base = RgbaImage::new(side, side);
    base.copy_from(&png, (side - png.width()) / 2, (side - png.height()) / 2)
        .expect("copiar logo para o quadrado");

    let mut frames = Vec::new();
    for tam in [16u32, 24, 32, 48, 64, 128] {
        if tam > side {
            continue; // nao amplia: o Windows escala o maior disponivel
        }
        let img = image::imageops::resize(&base, tam, tam, FilterType::Lanczos3);
        frames.push(
            IcoFrame::as_png(img.as_raw(), tam, tam, ExtendedColorType::Rgba8)
                .expect("frame do .ico"),
        );
    }

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("sagu.ico");
    let file = std::fs::File::create(&out).expect("criar sagu.ico");
    IcoEncoder::new(file)
        .encode_images(&frames)
        .expect("gravar sagu.ico");
    out
}
