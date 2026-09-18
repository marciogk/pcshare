//! Prova de ponta a ponta: dois clientes se acham por ID e conversam.
//!
//! Sobe um servidor de encontro de verdade num porta efêmera, registra um
//! host, conecta um viewer pelo ID, e confirma que os bytes atravessam o relay
//! nos dois sentidos. É o que separa "compila" de "funciona".

use std::time::{Duration, Instant};

use pcshare_core::quic::{QuicListener, QuicTransport};
use pcshare_core::{Channel, Transport};
use pcshare_encontro::{Mensagem, enviar, receber, servidor::servir};

/// Sobe o servidor numa thread e devolve o endereço em que ele escuta.
fn servidor_de_teste() -> std::net::SocketAddr {
    let escuta = QuicListener::bind("127.0.0.1:0".parse().unwrap()).expect("bind");
    let addr = escuta.local_addr().expect("local_addr");
    // `None`: modo aberto (sem cobrança), como nos testes de sinalização pura.
    std::thread::spawn(move || servir(escuta, None));
    addr
}

/// Espera bloqueante por uma mensagem de aplicação, com prazo.
fn esperar(t: &mut QuicTransport) -> (Channel, Vec<u8>) {
    let limite = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(v) = t.poll().expect("poll") {
            return v;
        }
        assert!(Instant::now() < limite, "nada chegou em 10 s");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn dois_clientes_se_acham_por_id_e_conversam() {
    let addr = servidor_de_teste();

    // Host registra-se e recebe a confirmação.
    let mut host = QuicTransport::connect(addr).expect("host conecta");
    enviar(&mut host, &Mensagem::Registrar("895 570 225".into())).expect("registrar");
    assert_eq!(receber(&mut host).expect("resposta"), Mensagem::RegistroOk);

    // Viewer pede o mesmo ID, escrito de outro jeito — o servidor normaliza.
    let mut viewer = QuicTransport::connect(addr).expect("viewer conecta");
    enviar(&mut viewer, &Mensagem::Conectar("895570225".into())).expect("conectar");

    // Os dois são avisados do emparelhamento, agora com o endereço público do
    // outro (para tentarem a conexão direta). Aqui só conferimos que veio o
    // sinal; o caminho testado adiante é o relay.
    assert!(matches!(
        receber(&mut viewer).expect("empar. viewer"),
        Mensagem::EmparelhadoCom(_)
    ));
    assert!(matches!(
        receber(&mut host).expect("empar. host"),
        Mensagem::EmparelhadoCom(_)
    ));

    // A partir daqui o servidor é cano. O que o host manda, o viewer recebe.
    host.send(Channel::Control, b"do host para o viewer").unwrap();
    assert_eq!(
        esperar(&mut viewer),
        (Channel::Control, b"do host para o viewer".to_vec())
    );

    // E o contrário — inclusive pelo canal de mídia, que preserva a semântica.
    viewer.send(Channel::Media, b"do viewer para o host").unwrap();
    assert_eq!(
        esperar(&mut host),
        (Channel::Media, b"do viewer para o host".to_vec())
    );
}

#[test]
fn id_inexistente_recebe_indisponivel() {
    let addr = servidor_de_teste();
    let mut viewer = QuicTransport::connect(addr).expect("viewer conecta");
    enviar(&mut viewer, &Mensagem::Conectar("000 000 000".into())).expect("conectar");
    assert_eq!(receber(&mut viewer).expect("resposta"), Mensagem::Indisponivel);
}

/// Um keyframe passa de 100 KB. O relay tem de deixar passar inteiro, sem teto
/// que corte no meio.
#[test]
fn quadro_grande_atravessa_o_relay() {
    let addr = servidor_de_teste();

    let mut host = QuicTransport::connect(addr).expect("host");
    enviar(&mut host, &Mensagem::Registrar("111 222 333".into())).unwrap();
    assert_eq!(receber(&mut host).unwrap(), Mensagem::RegistroOk);

    let mut viewer = QuicTransport::connect(addr).expect("viewer");
    enviar(&mut viewer, &Mensagem::Conectar("111222333".into())).unwrap();
    assert!(matches!(receber(&mut viewer).unwrap(), Mensagem::EmparelhadoCom(_)));
    assert!(matches!(receber(&mut host).unwrap(), Mensagem::EmparelhadoCom(_)));

    let grande: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    host.send(Channel::Media, &grande).unwrap();
    let (canal, recebido) = esperar(&mut viewer);
    assert_eq!(canal, Channel::Media);
    assert_eq!(recebido, grande, "quadro chegou diferente pelo relay");
}

/// O caminho novo, ponta a ponta: `registrar_e_esperar` + `conectar_por_id`
/// negociam pelo servidor e sobem a conexão. No loopback não há NAT, então a
/// **direta** vence — e os dois trocam dados sem depender do relay. Prova que a
/// orquestração (sinalização → endereços → furo) está ligada certo.
#[test]
fn helpers_negociam_e_conectam() {
    let addr = servidor_de_teste();

    let host_thread = std::thread::spawn(move || {
        pcshare_encontro::registrar_e_esperar(addr, "500 600 700").expect("host registra e espera")
    });
    // Deixa o host registrar antes de o viewer pedir o ID.
    std::thread::sleep(Duration::from_millis(500));

    let mut viewer =
        pcshare_encontro::conectar_por_id(addr, "500600700", None).expect("viewer conecta");
    let mut host = host_thread.join().unwrap();

    viewer.send(Channel::Control, b"ida").unwrap();
    assert_eq!(esperar(&mut host), (Channel::Control, b"ida".to_vec()));
    host.send(Channel::Media, b"volta").unwrap();
    assert_eq!(esperar(&mut viewer), (Channel::Media, b"volta".to_vec()));
}
