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

use std::time::{Duration, Instant};

use pcshare_capture::desktop;
use pcshare_capture::DesktopCapture;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
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

    let (sessao, console) = desktop::sessions();

    println!("=== agente [{rotulo}] ===");
    println!("pid            : {}", std::process::id());
    println!("sessão         : {sessao}  (console: {console})");
    println!("desktop de entrada: {}", desktop::input_desktop().label());

    // A pergunta que o passo 2 precisa responder: a captura funciona a partir
    // daqui? No desktop do usuário, sim. No Winlogon, só se o agente tiver
    // sido lançado pelo serviço com privilégio de SYSTEM.
    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => {
            println!("captura        : ABRIU  {}x{}", c.width(), c.height());
            Some(c)
        }
        Err(e) => {
            println!("captura        : FALHOU  {e}");
            None
        }
    };

    // Prova que a captura não só abre, mas entrega quadros de fato — abrir e
    // nunca receber nada seria um falso positivo.
    if let Some(cap) = cap.as_mut() {
        let ate = Instant::now() + Duration::from_secs(segundos.min(30));
        let mut quadros = 0u32;
        let mut perdas = 0u32;
        while Instant::now() < ate {
            match cap.next_frame(100) {
                Ok(Some(_)) => quadros += 1,
                Ok(None) => {}
                Err(_) => {
                    perdas += 1;
                    let _ = cap.recover();
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        println!("quadros em {segundos}s : {quadros}  (perdas de sessão: {perdas})");
        println!(
            "veredito       : {}",
            if quadros > 0 {
                "CAPTURA FUNCIONA NESTE DESKTOP"
            } else {
                "abriu mas não entregou nada"
            }
        );
    }

    println!("=== fim do agente [{rotulo}] ===");
}
