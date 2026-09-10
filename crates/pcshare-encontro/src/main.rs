//! Servidor de encontro do PCShare.
//!
//! Casa duas máquinas pelo ID e repassa bytes cifrados entre elas. Roda numa
//! máquina com endereço público; host e viewer conectam **de saída** até aqui,
//! e por isso atravessam o NAT dos dois lados sem configuração.
//!
//! A lógica vive em `servidor.rs`, na biblioteca, para poder ser testada. Aqui
//! só se lê o endereço e se sobe.
//!
//!     pcshare-encontro [endereço]      # padrão 0.0.0.0:7100
//!     PCSHARE_ENCONTRO_BIND=... pcshare-encontro

use std::net::SocketAddr;

use pcshare_core::quic::QuicListener;
use pcshare_encontro::servidor::servir;

const BIND_PADRAO: &str = "0.0.0.0:7100";

fn main() {
    let endereco = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("PCSHARE_ENCONTRO_BIND").ok())
        .unwrap_or_else(|| BIND_PADRAO.to_string());

    let endereco: SocketAddr = match endereco.parse() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("endereço inválido '{endereco}': {e}");
            std::process::exit(2);
        }
    };

    let escuta = match QuicListener::bind(endereco) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("não consegui escutar em {endereco}: {e}");
            std::process::exit(1);
        }
    };

    println!("pcshare-encontro escutando em {endereco}");
    servir(escuta);
}
