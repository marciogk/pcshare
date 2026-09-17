//! Ferramenta de administração das licenças, embutida no próprio binário do
//! servidor (`pcshare-encontro licenca ...`).
//!
//! Trabalha no **mesmo arquivo** que o servidor lê para cobrar, no mesmo
//! formato (`chave;validade;status;nota`), então não há risco de os dois
//! divergirem. Emissão manual continua sendo o modelo; isto só a torna segura e
//! sem digitar linha à mão. Um webhook de pagamento, na Fase 2, chamará
//! `emitir` (ou fará o mesmo append) para automatizar.
//!
//! ```text
//! pcshare-encontro licenca emitir --dias 30 --nota "cliente@email"
//! pcshare-encontro licenca listar
//! pcshare-encontro licenca renovar ABCD-... --dias 30
//! pcshare-encontro licenca revogar ABCD-...
//! ```
//!
//! Arquivo: `--arquivo <caminho>`, ou `PCSHARE_LICENCAS`, ou `licencas.txt`.

use std::collections::HashSet;
use std::io::Write;
use std::path::PathBuf;

use crate::licencas::{dias_de_texto, formatar_data, hoje_em_dias};

/// Ponto de entrada. `args` são os argumentos **depois** de `licenca`. Devolve
/// o código de saída do processo.
pub fn executar(args: &[String]) -> i32 {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let resto = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "emitir" => emitir(resto),
        "listar" => listar(resto),
        "renovar" => renovar(resto),
        "revogar" => revogar(resto),
        _ => {
            ajuda();
            2
        }
    }
}

fn ajuda() {
    eprintln!(
        "uso: pcshare-encontro licenca <comando>\n\
         \n  emitir  --dias N | --meses N   [--nota TEXTO]   emite uma chave nova\n\
         \n  listar                                         mostra todas\n\
         \n  renovar <chave> --dias N | --meses N           estende a validade\n\
         \n  revogar <chave>                                desativa\n\
         \narquivo: --arquivo CAMINHO, ou PCSHARE_LICENCAS, ou licencas.txt"
    );
}

fn emitir(args: &[String]) -> i32 {
    let caminho = caminho(args);
    let dias = dias(args);
    let nota = opt(args, "--nota").unwrap_or("").replace([';', '\n', '\r'], ",");
    let validade = formatar_data(hoje_em_dias() + dias);

    let existentes = chaves_existentes(&caminho);
    let mut chave = gerar_chave();
    while existentes.contains(&chave) {
        chave = gerar_chave();
    }

    let linha = format!("{chave};{validade};ativa;{}", nota.trim());
    if let Err(e) = anexar(&caminho, &linha) {
        eprintln!("erro ao gravar em {}: {e}", caminho.display());
        return 1;
    }
    println!("Licença emitida:");
    println!("  chave    : {chave}");
    println!("  validade : {validade}  ({dias} dias)");
    if !nota.trim().is_empty() {
        println!("  nota     : {}", nota.trim());
    }
    println!("  arquivo  : {}", caminho.display());
    0
}

fn listar(args: &[String]) -> i32 {
    let caminho = caminho(args);
    let conteudo = std::fs::read_to_string(&caminho).unwrap_or_default();
    let hoje = hoje_em_dias();
    println!(
        "{:<21} {:<12} {:<10} {:>7}  nota",
        "chave", "validade", "status", "restam"
    );
    let mut n = 0;
    for linha in conteudo.lines() {
        let Some(c) = campos(linha) else { continue };
        n += 1;
        let restam = match dias_de_texto(&c.validade) {
            Some(d) => format!("{}d", d - hoje),
            None => "—".to_string(),
        };
        println!(
            "{:<21} {:<12} {:<10} {:>7}  {}",
            c.chave, c.validade, c.status, restam, c.nota
        );
    }
    if n == 0 {
        println!("(nenhuma licença em {})", caminho.display());
    }
    0
}

fn renovar(args: &[String]) -> i32 {
    let Some(alvo) = posicional(args) else {
        eprintln!("uso: pcshare-encontro licenca renovar <chave> --dias N");
        return 2;
    };
    let caminho = caminho(args);
    let dias = dias(args);
    let hoje = hoje_em_dias();
    let achou = reescrever(&caminho, &alvo, |c| {
        // Estende a partir do que for maior: hoje, ou a validade atual (assim
        // renovar antes de vencer soma, em vez de encurtar).
        let base = dias_de_texto(&c.validade).unwrap_or(hoje).max(hoje);
        let nova = formatar_data(base + dias);
        format!("{};{};ativa;{}", c.chave, nova, c.nota)
    });
    relatar(achou, &alvo, "renovada", &caminho)
}

fn revogar(args: &[String]) -> i32 {
    let Some(alvo) = posicional(args) else {
        eprintln!("uso: pcshare-encontro licenca revogar <chave>");
        return 2;
    };
    let caminho = caminho(args);
    let achou = reescrever(&caminho, &alvo, |c| {
        format!("{};{};revogada;{}", c.chave, c.validade, c.nota)
    });
    relatar(achou, &alvo, "revogada", &caminho)
}

fn relatar(achou: Result<bool, std::io::Error>, alvo: &str, verbo: &str, caminho: &PathBuf) -> i32 {
    match achou {
        Ok(true) => {
            println!("Licença {alvo} {verbo}.");
            0
        }
        Ok(false) => {
            eprintln!("chave não encontrada: {alvo}");
            1
        }
        Err(e) => {
            eprintln!("erro ao gravar em {}: {e}", caminho.display());
            1
        }
    }
}

// ---- ajudantes ----

struct Campos {
    chave: String,
    validade: String,
    status: String,
    nota: String,
}

/// Interpreta uma linha de licença. `None` para comentário/linha em branco.
fn campos(linha: &str) -> Option<Campos> {
    let t = linha.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    let mut p = t.split(';');
    let chave = p.next().unwrap_or("").trim().to_string();
    if chave.is_empty() {
        return None;
    }
    Some(Campos {
        chave,
        validade: p.next().unwrap_or("").trim().to_string(),
        status: p.next().unwrap_or("ativa").trim().to_string(),
        nota: p.next().unwrap_or("").trim().to_string(),
    })
}

/// Reescreve o arquivo, transformando só a linha da chave `alvo` e **preservando
/// comentários e demais linhas**. Devolve se achou a chave.
fn reescrever(
    caminho: &PathBuf,
    alvo: &str,
    transform: impl Fn(&Campos) -> String,
) -> Result<bool, std::io::Error> {
    let conteudo = std::fs::read_to_string(caminho)?;
    let mut achou = false;
    let mut saida = String::new();
    for linha in conteudo.lines() {
        match campos(linha) {
            Some(c) if c.chave == alvo => {
                achou = true;
                saida.push_str(&transform(&c));
            }
            _ => saida.push_str(linha),
        }
        saida.push('\n');
    }
    if achou {
        std::fs::write(caminho, saida)?;
    }
    Ok(achou)
}

fn chaves_existentes(caminho: &PathBuf) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(c) = std::fs::read_to_string(caminho) {
        for linha in c.lines() {
            if let Some(c) = campos(linha) {
                set.insert(c.chave);
            }
        }
    }
    set
}

fn anexar(caminho: &PathBuf, linha: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(caminho)?;
    writeln!(f, "{linha}")
}

fn caminho(args: &[String]) -> PathBuf {
    opt(args, "--arquivo")
        .map(PathBuf::from)
        .or_else(|| std::env::var("PCSHARE_LICENCAS").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("licencas.txt"))
}

/// Valor de uma opção `--nome VALOR`.
fn opt<'a>(args: &'a [String], nome: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == nome)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// Primeiro argumento que não é opção (a chave, em renovar/revogar).
fn posicional(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            i += 2; // pula a opção e o valor dela
        } else {
            return Some(a.clone());
        }
    }
    None
}

/// Duração em dias: `--dias N`, ou `--meses N` (×30), padrão 30.
fn dias(args: &[String]) -> i64 {
    if let Some(d) = opt(args, "--dias").and_then(|s| s.parse::<i64>().ok()) {
        return d.max(1);
    }
    if let Some(m) = opt(args, "--meses").and_then(|s| s.parse::<i64>().ok()) {
        return m.max(1) * 30;
    }
    30
}

/// Chave no formato `XXXX-XXXX-XXXX-XXXX`, alfabeto sem caracteres ambíguos
/// (0/O, 1/I/L) — dita-se e digita-se sem confusão.
fn gerar_chave() -> String {
    use rand::RngExt;
    const ALFABETO: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let mut s = String::with_capacity(19);
    for g in 0..4 {
        if g > 0 {
            s.push('-');
        }
        for _ in 0..4 {
            let i = rng.random_range(0..ALFABETO.len());
            s.push(ALFABETO[i] as char);
        }
    }
    s
}
