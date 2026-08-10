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
    let pasta = pcshare_capture::linha_do_tempo::pasta();
    let Ok(entradas) = std::fs::read_dir(&pasta) else {
        eprintln!("linha do tempo não encontrada em {}", pasta.display());
        eprintln!("rode o serviço e provoque ao menos uma troca de desktop.");
        return;
    };

    // Um arquivo por processo, para que cada um seja dono do seu e não haja
    // disputa de permissão entre SYSTEM e usuário.
    let mut texto = String::new();
    let mut arquivos = 0usize;
    for e in entradas.flatten() {
        if e.path().extension().is_some_and(|x| x == "tsv")
            && let Ok(t) = std::fs::read_to_string(e.path())
        {
            texto.push_str(&t);
            arquivos += 1;
        }
    }
    if arquivos == 0 {
        eprintln!("nenhum arquivo de linha do tempo em {}", pasta.display());
        return;
    }

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
    println!("origem: {} ({arquivos} processos)", pasta.display());
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

    // Verificação de sanidade do próprio instrumento, antes de qualquer
    // número: todo lançamento tem de ser seguido pelo nascimento de um agente.
    // Quando não é, o agente existiu mas não conseguiu registrar — e casar os
    // eventos restantes produziria pares entre agentes que nunca se
    // sucederam. Foi assim que apareceram 10 segundos de cegueira inexistente.
    let mut lancamentos_mudos = Vec::new();
    for (i, e) in eventos.iter().enumerate() {
        if e.tipo.starts_with("lancou-") {
            let houve_inicio = eventos[i + 1..]
                .iter()
                .take_while(|p| !p.tipo.starts_with("lancou-"))
                .any(|p| p.tipo == "inicio");
            if !houve_inicio {
                lancamentos_mudos.push(e.tipo.trim_start_matches("lancou-").to_string());
            }
        }
    }
    if !lancamentos_mudos.is_empty() {
        println!("{:=<74}", "");
        println!("MEDIÇÃO INVÁLIDA");
        println!("{:=<74}", "");
        println!(
            "  {} lançamento(s) não produziram evento algum: {}",
            lancamentos_mudos.len(),
            lancamentos_mudos.join(", ")
        );
        println!("\n  Esses agentes rodaram — o log do supervisor comprova — mas não");
        println!("  conseguiram escrever na linha do tempo. Casar os eventos que");
        println!("  sobraram produziria pares entre agentes que nunca se sucederam.");
        println!("\n  Verifique permissões de escrita em {}", pasta.display());
        return;
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

