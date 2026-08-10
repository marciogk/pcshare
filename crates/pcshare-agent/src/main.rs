//! Agente do PCShare — o processo que enxerga a tela.
//!
//! Existe por causa da decisão D5. Um serviço do Windows roda na sessão 0, que
//! não tem interface gráfica desde o Vista: ele não vê tela nenhuma. Quem vê é
//! o agente, lançado pelo serviço **dentro** da sessão do usuário e amarrado a
//! um desktop específico.
//!
//! A amarração é o ponto. Um processo do Windows nasce preso ao desktop
//! indicado em `STARTUPINFO.lpDesktop` e não troca depois. É por isso que a
//! resposta ao Secure Desktop é lançar **outro** agente, não reconfigurar o que
//! já existe.
//!
//! Medido em 05/08/2026: um processo comum falha 100% das vezes ao tentar
//! duplicar o `Winlogon` — `0x80070005`, acesso negado. Ver `docs/FASE-3.md`.
//!
//! Neste passo o agente só **relata**: em que desktop caiu, em que sessão, e
//! se a captura abre ali. É o que prova o mecanismo de lançamento antes de
//! mover a pipeline inteira para dentro dele.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use pcshare_capture::DesktopCapture;
use pcshare_capture::desktop;

/// Registra em arquivo, e no console **se** houver um.
///
/// O agente é lançado pelo supervisor sem console. Nesse contexto `println!`
/// entra em pânico ao falhar a escrita, e o processo morre no primeiro relato
/// — foi exatamente o que aconteceu: o supervisor relançava o agente a cada
/// 100 ms sem perceber que ele nascia e morria.
fn log(rotulo: &str, msg: &str) {
    let linha = format!("[{rotulo}] {msg}\n");
    // Ignora falha de propósito: sem console não há para onde escrever, e
    // isso não é motivo para derrubar o agente.
    let _ = std::io::stdout().write_all(linha.as_bytes());
    let _ = std::io::stdout().flush();

    let caminho = caminho_do_log(rotulo);
    if let Some(dir) = caminho.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&caminho)
    {
        let _ = f.write_all(linha.as_bytes());
    }
}

/// Linha do tempo compartilhada por **todos** os agentes.
///
/// A cegueira que interessa acontece *entre* dois processos: do último quadro
/// que o agente morrendo entregou até o primeiro que o agente nascendo
/// entrega. Nenhum dos dois consegue medir isso sozinho — por isso um arquivo
/// só, com carimbo de relógio de parede, que é o único relógio que dois
/// processos compartilham.
///
/// Formato, separado por tabulação:
/// `microssegundos_desde_epoch  pid  desktop  evento`
use pcshare_capture::canal;
use pcshare_capture::desktop::InputDesktop;
use pcshare_capture::linha_do_tempo::{agora_us, registrar as linha_do_tempo};

/// Nome que o Windows usa, para relatar ao supervisor.
fn nome_do_desktop(d: &InputDesktop) -> &str {
    match d {
        InputDesktop::Default => "Default",
        // `Denied` é o que se vê quando o Secure Desktop está à frente.
        InputDesktop::Winlogon | InputDesktop::Denied => "Winlogon",
        InputDesktop::ScreenSaver => "Screen-saver",
        InputDesktop::Other(n) => n,
    }
}

fn caminho_do_log(rotulo: &str) -> PathBuf {
    // `ProgramData` porque o agente pode nascer de um serviço SYSTEM, que não
    // tem perfil de usuário.
    let seguro: String = rotulo
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join(format!("agente-{seguro}.log"))
}

fn main() {
    // Primeiríssima instrução: este carimbo mede o custo de criação do
    // processo, e qualquer coisa antes dele o contamina. A versão anterior o
    // escrevia depois de quatro gravações em disco, e assim media o log em
    // vez do nascimento.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vinculo_cru = args
        .iter()
        .position(|a| a == "--desktop")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "desconhecido".into());
    let instrumento_ok = linha_do_tempo(&vinculo_cru, "inicio");
    let rotulo = args
        .iter()
        .position(|a| a == "--rotulo")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "sem-rótulo".into());
    let segundos: u64 = args
        .iter()
        .position(|a| a == "--segundos")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    // A qual desktop este agente foi amarrado no nascimento. É contra isto
    // que se compara o desktop de entrada para detectar a troca.
    let vinculo = args
        .iter()
        .position(|a| a == "--desktop")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| rotulo.clone());
    let codigo_do_vinculo = match vinculo.as_str() {
        "Default" => desktop::codigo::DEFAULT,
        "Winlogon" => desktop::codigo::WINLOGON,
        "Screen-saver" => desktop::codigo::SCREENSAVER,
        _ => desktop::codigo::OUTRO,
    };

    let (sessao, console) = desktop::sessions();

    log(&rotulo, "=== início ===");
    if !instrumento_ok {
        // Sem este aviso, o agente some da medição sem deixar rastro — e o
        // analisador passa a casar eventos de agentes errados.
        log(
            &rotulo,
            "AVISO: não consegui escrever na linha do tempo; a medição ficará incompleta",
        );
    }
    log(&rotulo, &format!("pid {}", std::process::id()));
    log(&rotulo, &format!("sessão {sessao} (console: {console})"));
    log(
        &rotulo,
        &format!("desktop de entrada: {}", desktop::input_desktop().label()),
    );

    // A pergunta que o passo 2 precisa responder: a captura funciona a partir
    // daqui? No desktop do usuário, sim. No Winlogon, só se o agente tiver
    // sido lançado pelo serviço com privilégio de SYSTEM.
    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => {
            log(&rotulo, &format!("captura ABRIU {}x{}", c.width(), c.height()));
            // Se o primeiro registro passou, os seguintes passam: mesmo
            // arquivo, mesmo dono.
            let _ = linha_do_tempo(&vinculo, "captura-abriu");
            Some(c)
        }
        Err(e) => {
            log(&rotulo, &format!("captura FALHOU: {e}"));
            let _ = linha_do_tempo(&vinculo, "captura-falhou");
            None
        }
    };

    // Prova que a captura não só abre, mas entrega quadros de fato — abrir e
    // nunca receber nada seria um falso positivo.
    if let Some(cap) = cap.as_mut() {
        // `--segundos 0` roda até ser encerrado: é como o supervisor usa o
        // agente, já que quem decide o fim da vida dele é a troca de desktop.
        let perpetuo = segundos == 0;
        let ate = Instant::now() + Duration::from_secs(segundos.clamp(1, 3600));
        let mut quadros = 0u32;
        let mut perdas = 0u32;
        let mut ultimo_quadro_us: Option<u128> = None;
        let mut capturando = false;
        let mut ultimo_relatado = codigo_do_vinculo;
        let mut proximo_relato = Instant::now() + Duration::from_secs(2);

        while perpetuo || Instant::now() < ate {
            // O agente está preso a um desktop e não pode trocar. Quando o
            // desktop de entrada vira outro, ele deixa de servir para
            // qualquer coisa: fica cego, acumulando perdas.
            //
            // Observado: 78 quadros congelados e as perdas subindo para
            // sempre, porque ninguém o encerrou. Quem percebe a troca é ele
            // — o supervisor, na sessão 0, não enxerga desktop nenhum. Então
            // é ele quem avisa, encerrando com o código do desktop novo.
            // Relata a troca **sem morrer**.
            //
            // Antes o agente encerrava e o supervisor criava outro no caminho
            // crítico. Medido: recriar com token de usuário custa cerca de
            // 700 ms — 90% da cegueira ao voltar do UAC. Vivo, ele fica cego
            // por alguns segundos e volta sozinho quando o desktop retorna.
            let agora = desktop::input_desktop();
            let codigo_agora = desktop::codigo::de(&agora);
            if perpetuo && codigo_agora != ultimo_relatado {
                ultimo_relatado = codigo_agora;
                let nome = nome_do_desktop(&agora);
                if canal::enviar(&canal::mensagem_de_desktop(nome)) {
                    log(&rotulo, &format!("relatei ao supervisor: desktop '{nome}'"));
                } else {
                    // Sem canal, volta ao comportamento antigo: encerrar com o
                    // destino no código de saída. Mais lento, mas o supervisor
                    // continua sabendo para onde ir.
                    log(
                        &rotulo,
                        "canal indisponível — encerrando para relatar pelo código de saída",
                    );
                    if let Some(us) = ultimo_quadro_us {
                        let _ = linha_do_tempo(&vinculo, &format!("cego-desde {us}"));
                    }
                    std::process::exit(codigo_agora as i32);
                }
            }

            if perpetuo && Instant::now() >= proximo_relato {
                log(
                    &rotulo,
                    &format!("vivo em '{}' | {quadros} quadros | {perdas} perdas", agora.label()),
                );
                proximo_relato += Duration::from_secs(2);
            }
            match cap.next_frame(100) {
                Ok(Some(_)) => {
                    quadros += 1;
                    ultimo_quadro_us = Some(agora_us());
                    // Marca a **transição** para capturando, não o primeiro
                    // quadro da vida do processo: com agentes persistentes,
                    // quem fecha a janela de cegueira costuma ser um agente
                    // que retoma, não um que nasce.
                    if !capturando {
                        capturando = true;
                        let _ = linha_do_tempo(&vinculo, "capturando");
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    perdas += 1;
                    if capturando {
                        capturando = false;
                        // Abre a janela de cegueira no instante do último
                        // quadro entregue, que é quando a tela remota
                        // congelou de fato.
                        if let Some(us) = ultimo_quadro_us {
                            let _ = linha_do_tempo(&vinculo, &format!("cego-desde {us}"));
                        }
                    }
                    let _ = cap.recover();
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        log(
            &rotulo,
            &format!("quadros {quadros} (perdas de sessão: {perdas})"),
        );
        log(
            &rotulo,
            if quadros > 0 {
                "veredito: CAPTURA FUNCIONA NESTE DESKTOP"
            } else {
                "veredito: abriu mas não entregou nada"
            }
        );
    }

    log(&rotulo, "=== fim ===");
}
