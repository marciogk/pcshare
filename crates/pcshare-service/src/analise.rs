//! Calcula a cegueira real a partir da linha do tempo dos agentes.
//!
//! # O que se está medindo, exatamente
//!
//! A cegueira que interessa acontece **entre dois processos**: do último
//! quadro que o agente morrendo entregou até o primeiro que o agente nascendo
//! entrega. É o intervalo em que a tela remota fica congelada — nenhum dos
//! dois agentes consegue medi-lo sozinho.
//!
//! Por isso todos escrevem numa linha do tempo comum, com carimbo de relógio
//! de parede, que é o único relógio que processos distintos compartilham.
//!
//! # Por que não somar intervalos parciais
//!
//! O log do supervisor mostra "13,4 ms desde a percepção", e é tentador
//! chamar isso de resultado. Não é: falta o tempo até o agente **perceber** a
//! troca e o tempo até o novo abrir a captura. Somar pedaços foi exatamente o
//! que manteve os "16,8 ms" de latência vivos por dois dias neste projeto.

use std::path::PathBuf;

/// Onde cada fatia do tempo foi gasta, em milissegundos.
struct Fases {
    perceber: f64,
    nascer: f64,
    captura: f64,
    quadro: f64,
}

struct Evento {
    us: u128,
    pid: u32,
    desktop: String,
    tipo: String,
}

pub fn analisar() {
    let caminho = caminho_da_linha_do_tempo();
    let Ok(texto) = std::fs::read_to_string(&caminho) else {
        eprintln!("linha do tempo não encontrada em {}", caminho.display());
        eprintln!("rode o serviço e provoque ao menos uma troca de desktop.");
        return;
    };

    // Descartar linha em silêncio é armadilha: uma marca de ordem de byte no
    // começo do arquivo já engoliu um evento num teste, e nada avisou.
    let mut ignoradas = 0usize;
    let mut eventos: Vec<Evento> = Vec::new();
    for bruta in texto.lines() {
        let l = bruta.trim_start_matches('\u{feff}').trim();
        if l.is_empty() {
            continue;
        }
        let mut c = l.split('\t');
        let lido = (|| {
            Some(Evento {
                us: c.next()?.trim().parse().ok()?,
                pid: c.next()?.trim().parse().ok()?,
                desktop: c.next()?.to_string(),
                tipo: c.next()?.to_string(),
            })
        })();
        match lido {
            Some(e) => eventos.push(e),
            None => ignoradas += 1,
        }
    }
    eventos.sort_by_key(|e| e.us);

    if eventos.is_empty() {
        eprintln!("linha do tempo vazia.");
        return;
    }

    println!("PCShare — cegueira medida entre agentes\n");
    println!("origem: {}", caminho.display());
    println!("{} eventos", eventos.len());
    if ignoradas > 0 {
        println!("{ignoradas} linha(s) ilegível(is) — a medição pode estar incompleta");
    }
    println!();

    // Percorre em ordem, casando cada "último quadro" com o "primeiro quadro"
    // do agente seguinte. É o par que define a janela.
    let mut aberto: Option<(&Evento, u128)> = None;
    let mut janelas: Vec<(f64, String, String)> = Vec::new();
    let mut fases: Vec<Fases> = Vec::new();
    // Marcos intermediários do episódio em curso.
    let mut lancou_us: Option<u128> = None;
    let mut inicio_us: Option<u128> = None;
    let mut captura_us: Option<u128> = None;

    for e in &eventos {
        if let Some(us) = e.tipo.strip_prefix("ultimo-quadro-em ")
            && let Ok(us) = us.trim().parse::<u128>()
        {
            aberto = Some((e, us));
            lancou_us = None;
            inicio_us = None;
            captura_us = None;
        } else if e.tipo.starts_with("lancou-") {
            lancou_us = Some(e.us);
        } else if e.tipo == "inicio" {
            inicio_us = Some(e.us);
        } else if e.tipo == "captura-abriu" {
            captura_us = Some(e.us);
        } else if e.tipo == "primeiro-quadro"
            && let Some((anterior, ultimo_us)) = aberto.take()
            && e.pid != anterior.pid
        {
            let ms = (e.us.saturating_sub(ultimo_us)) as f64 / 1000.0;
            janelas.push((ms, anterior.desktop.clone(), e.desktop.clone()));

            // Sem os marcos do supervisor, a decomposição não fecha; então
            // preenche-se com zero em vez de inventar.
            let d = |a: Option<u128>, b: Option<u128>| -> f64 {
                match (a, b) {
                    (Some(x), Some(y)) => (y.saturating_sub(x)) as f64 / 1000.0,
                    _ => 0.0,
                }
            };
            fases.push(Fases {
                perceber: d(Some(ultimo_us), lancou_us),
                nascer: d(lancou_us, inicio_us),
                captura: d(inicio_us, captura_us),
                quadro: d(captura_us, Some(e.us)),
            });
        }
    }

    if janelas.is_empty() {
        println!("Nenhuma janela de cegueira completa foi encontrada.");
        println!("É preciso uma troca de desktop com quadros dos dois lados —");
        println!("provoque um prompt de UAC com o serviço rodando.");
        return;
    }

    println!("{:=<74}", "");
    println!("JANELAS DE CEGUEIRA, decompostas");
    println!("{:=<74}", "");
    println!(
        "  {:<22} {:>8} {:>9} {:>9} {:>8} {:>8}",
        "transição", "total", "perceber", "nascer", "captura", "quadro"
    );
    for (i, (ms, de, para)) in janelas.iter().enumerate() {
        let f = &fases[i];
        println!(
            "  {:<22} {ms:>7.0}ms {:>8.0}ms {:>8.0}ms {:>7.0}ms {:>7.0}ms",
            format!("{de} para {para}"),
            f.perceber,
            f.nascer,
            f.captura,
            f.quadro
        );
    }
    println!("\n  perceber : agente sair, supervisor notar e chamar CreateProcess");
    println!("  nascer   : CreateProcess retornar até o agente rodar a 1ª instrução");
    println!("  captura  : abrir o dispositivo D3D11 e a duplicação");
    println!("  quadro   : da captura aberta até o primeiro quadro entregue");

    let mut ordenadas: Vec<f64> = janelas.iter().map(|(ms, _, _)| *ms).collect();
    ordenadas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mediana = ordenadas[ordenadas.len() / 2];

    println!("\n  mediana : {mediana:.1} ms");
    println!("  mínimo  : {:.1} ms", ordenadas[0]);
    println!("  máximo  : {:.1} ms", ordenadas[ordenadas.len() - 1]);

    println!("\n{:=<60}", "");
    println!("COMPARAÇÃO");
    println!("{:=<60}", "");
    println!("  sem serviço (passo 1) : 3950 a 5347 ms");
    println!("  com serviço           : {mediana:.0} ms (mediana)");
    if mediana > 0.0 {
        println!("  redução               : {:.0}x", 4600.0 / mediana);
    }
    println!("\n  Este número inclui tudo: o agente perceber a troca, encerrar,");
    println!("  o supervisor lançar o próximo, ele abrir a captura e entregar");
    println!("  o primeiro quadro.");
}

fn caminho_da_linha_do_tempo() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("linha-do-tempo.tsv")
}
