//! Embute o ícone do PCShare no executável (aparece no Explorer, na barra de
//! tarefas e no arquivo baixado). Só no Windows; falha vira aviso, não quebra o
//! build (na pior das hipóteses o .exe sai sem ícone).

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let ico = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/pcshare.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap());
    if let Err(e) = res.compile() {
        println!("cargo:warning=não foi possível embutir o ícone: {e}");
    }
}
