//! Linha do tempo compartilhada entre o supervisor e todos os agentes.
//!
//! A cegueira que interessa acontece **entre processos**, e nenhum deles a
//! enxerga sozinho. Por isso todos escrevem no mesmo arquivo, carimbando com
//! relógio de parede — o único relógio que processos distintos compartilham.
//!
//! Formato, separado por tabulação:
//! `microssegundos_desde_epoch  pid  contexto  evento`
//!
//! Vive aqui, e não em cada binário, porque instrumento de medição que existe
//! em duas cópias vira duas medições diferentes na primeira alteração.

use std::io::Write;
use std::path::PathBuf;

pub fn agora_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0)
}

pub fn registrar(contexto: &str, evento: &str) {
    let linha = format!(
        "{}\t{}\t{contexto}\t{evento}\n",
        agora_us(),
        std::process::id()
    );

    let caminho = caminho();
    if let Some(dir) = caminho.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&caminho)
    {
        let _ = f.write_all(linha.as_bytes());
        let _ = f.flush();
    }
}

pub fn caminho() -> PathBuf {
    // `ProgramData` porque o supervisor roda como SYSTEM, sem perfil de
    // usuário — e agente e supervisor precisam escrever no mesmo arquivo.
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("linha-do-tempo.tsv")
}
