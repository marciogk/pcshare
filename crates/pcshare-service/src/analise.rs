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


struct Evento {
    us: u128,
    pid: u32,
    desktop: String,
    tipo: String,
}

/// `pasta` permite analisar uma medição arquivada, em vez da corrente.
pub fn analisar(pasta: Option<String>) {
    let pasta = pasta
        .map(std::path::PathBuf::from)
        .unwrap_or_else(pcshare_capture::linha_do_tempo::pasta);
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
    // Com agentes persistentes, a cegueira é o intervalo em que **nenhum**
    // agente está capturando. Quem a fecha costuma ser um agente que retoma,
    // não um que nasce — por isso o pareamento é entre transições de estado,
    // e não entre nascimentos.
    let mut capturando: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut cego_desde: Option<(u128, String)> = None;
    let mut janelas: Vec<(f64, String, String)> = Vec::new();

    for e in &eventos {
        if e.tipo == "capturando" {
            let era_vazio = capturando.is_empty();
            capturando.insert(e.pid);
            if era_vazio
                && let Some((desde, de)) = cego_desde.take()
            {
                let ms = (e.us.saturating_sub(desde)) as f64 / 1000.0;
                janelas.push((ms, de, e.desktop.clone()));
            }
        } else if let Some(us) = e.tipo.strip_prefix("cego-desde ")
            && let Ok(us) = us.trim().parse::<u128>()
        {
            capturando.remove(&e.pid);
            // Só abre janela quando o último que capturava desiste: enquanto
            // houver um agente entregando quadros, a tela remota não congelou.
            if capturando.is_empty() && cego_desde.is_none() {
                cego_desde = Some((us, e.desktop.clone()));
            }
        }
    }

    // Verificação de sanidade do próprio instrumento, antes de qualquer
    // número: todo lançamento tem de ser seguido pelo nascimento de um agente.
    // Quando não é, o agente existiu mas não conseguiu registrar — e casar os
    // eventos restantes produziria pares entre agentes que nunca se
    // sucederam. Foi assim que apareceram 10 segundos de cegueira inexistente.
    //
    // Conta por desktop, em vez de exigir que cada lançamento seja seguido
    // pelo seu nascimento. A versão anterior olhava só até o próximo
    // `lancou-`, o que pressupunha lançamentos serializados — e deixou de
    // valer quando o supervisor passou a manter o `Winlogon` quente: dois
    // lançamentos ficam em voo ao mesmo tempo, o segundo aparece antes do
    // `inicio` do primeiro, e uma medição perfeitamente boa seria recusada.
    //
    // Contar é indiferente à ordem e continua pegando o caso que importa: um
    // agente que rodou mas não conseguiu escrever na linha do tempo.
    let mut lancados: std::collections::HashMap<&str, i32> = std::collections::HashMap::new();
    for e in &eventos {
        if let Some(d) = e.tipo.strip_prefix("lancou-") {
            *lancados.entry(d).or_default() += 1;
        } else if e.tipo == "inicio" {
            *lancados.entry(e.desktop.as_str()).or_default() -= 1;
        }
    }
    let mut lancamentos_mudos: Vec<String> = lancados
        .into_iter()
        .filter(|&(_, saldo)| saldo > 0)
        .map(|(d, saldo)| {
            if saldo > 1 {
                format!("{d} ({saldo}x)")
            } else {
                d.to_string()
            }
        })
        .collect();
    lancamentos_mudos.sort();
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

    println!("{:=<60}", "");
    println!("JANELAS DE CEGUEIRA");
    println!("{:=<60}", "");
    println!("  Intervalos em que nenhum agente estava entregando quadros.\n");
    for (i, (ms, de, para)) in janelas.iter().enumerate() {
        println!("  {:>2}. {de} para {para}: {ms:.1} ms", i + 1);
    }

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
    println!("\n  Mede o tempo em que a tela remota fica congelada: do último");
    println!("  quadro entregue por qualquer agente até o próximo quadro de");
    println!("  qualquer agente. Com agentes persistentes, quem fecha a janela");
    println!("  costuma ser um agente que retoma, não um que nasce.");
}

