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

/// `false` quando não foi possível registrar — e isso **precisa** ser tratado.
///
/// A versão anterior engolia a falha. Um arquivo único, criado pelo supervisor
/// como SYSTEM, herdava permissão de leitura apenas para o grupo Usuários: os
/// agentes que rodam como usuário não conseguiam escrever, e sumiam da
/// medição em silêncio. O analisador então casava eventos de agentes errados e
/// produzia números com cara de dado — 10 segundos de "cegueira" que nunca
/// existiram.
#[must_use]
pub fn registrar(contexto: &str, evento: &str) -> bool {
    let linha = format!(
        "{}\t{}\t{contexto}\t{evento}\n",
        agora_us(),
        std::process::id()
    );

    let pasta = pasta();
    if std::fs::create_dir_all(&pasta).is_err() {
        return false;
    }

    // Um arquivo por processo: cada um é dono do seu, e não há disputa de
    // permissão entre SYSTEM e usuário. O analisador junta tudo depois.
    let caminho = pasta.join(format!("{}.tsv", std::process::id()));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&caminho)
    {
        Ok(mut f) => f.write_all(linha.as_bytes()).and_then(|_| f.flush()).is_ok(),
        Err(_) => false,
    }
}

/// Pasta com um arquivo por processo participante.
pub fn pasta() -> PathBuf {
    // `ProgramData` porque o supervisor roda como SYSTEM, sem perfil de
    // usuário, e os dois precisam escrever no mesmo lugar.
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("linha-do-tempo")
}

/// `HH:MM:SS.mmm` em horário local.
///
/// Existe porque o log do agente não carimbava a hora, e contar linhas nele
/// misturava execuções — em 19/08/2026 isso quase produziu a conclusão de que
/// uma correção tinha falhado, quando o que se via era acúmulo de dias
/// anteriores. Um registro sem hora é um registro que mente por omissão.
pub fn hora_local() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let s = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s.wHour, s.wMinute, s.wSecond, s.wMilliseconds
    )
}
