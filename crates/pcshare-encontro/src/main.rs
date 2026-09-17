//! Servidor de encontro do PCShare.
//!
//! Casa duas máquinas pelo ID e repassa bytes cifrados entre elas. Roda numa
//! máquina com endereço público; host e viewer conectam **de saída** até aqui,
//! e por isso atravessam o NAT dos dois lados sem configuração.
//!
//! A lógica vive em `servidor.rs`, na biblioteca, para poder ser testada. Aqui
//! só se lê o endereço e se sobe.
//!
//!     pcshare-encontro [endereço]                       # padrão 0.0.0.0:7100
//!     pcshare-encontro 0.0.0.0:7100 --licencas /caminho/licencas.txt
//!     PCSHARE_ENCONTRO_BIND=... PCSHARE_LICENCAS=... pcshare-encontro
//!
//! Sem `--licencas` (nem `PCSHARE_LICENCAS`), roda em **modo aberto**: qualquer
//! viewer conecta, de graça — o comportamento de sempre. Com um arquivo de
//! licenças, entra em **cobrança**: só viewers com licença válida conectam.

use std::net::SocketAddr;

use pcshare_core::quic::QuicListener;
use pcshare_encontro::licencas::Licencas;
use pcshare_encontro::servidor::servir;

const BIND_PADRAO: &str = "0.0.0.0:7100";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Subcomando de administração de licenças, no mesmo binário: emitir/listar/
    // renovar/revogar chaves. Não sobe o servidor.
    if args.first().map(String::as_str) == Some("licenca") {
        std::process::exit(pcshare_encontro::admin::executar(&args[1..]));
    }

    // Primeiro argumento posicional (que não seja uma opção) é o endereço.
    let endereco = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .or_else(|| std::env::var("PCSHARE_ENCONTRO_BIND").ok())
        .unwrap_or_else(|| BIND_PADRAO.to_string());

    let endereco: SocketAddr = match endereco.parse() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("endereço inválido '{endereco}': {e}");
            std::process::exit(2);
        }
    };

    // Caminho do arquivo de licenças: `--licencas <caminho>` ou PCSHARE_LICENCAS.
    let caminho_licencas = args
        .iter()
        .position(|a| a == "--licencas")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("PCSHARE_LICENCAS").ok());
    let licencas = caminho_licencas.map(Licencas::do_arquivo);

    let escuta = match QuicListener::bind(endereco) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("não consegui escutar em {endereco}: {e}");
            std::process::exit(1);
        }
    };

    println!("pcshare-encontro escutando em {endereco}");
    servir(escuta, licencas);
}
