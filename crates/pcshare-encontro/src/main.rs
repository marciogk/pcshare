//! Servidor de encontro do PCShare (esqueleto).
//!
//! Casa duas máquinas pelo ID e repassa bytes cifrados entre elas. Roda numa
//! máquina com endereço público; host e viewer conectam **de saída** até aqui,
//! e por isso atravessam o NAT dos dois lados sem configuração.
//!
//! O protocolo já existe em `lib.rs`. O que falta, e vem no próximo passo:
//!
//!   - aceitar conexões num `QuicListener`;
//!   - ler o primeiro `Mensagem` de cada uma: `Registrar` guarda o host num
//!     mapa `id → conexão`; `Conectar` procura o par;
//!   - ao casar, responder `Emparelhado` aos dois e repassar cada
//!     `(Channel, payload)` de um lado para o outro até a sessão cair;
//!   - `Indisponivel` quando o ID não está registrado, `Ocupado` quando já há
//!     sessão.
//!
//! Nada disso decifra a sessão: o repasse é de payloads que o Noise já cifrou.

fn main() {
    eprintln!(
        "pcshare-encontro: servidor de sinalização e repasse.\n\
         O protocolo está pronto (lib.rs); o servidor é o próximo passo.\n\
         Ver docs/PROXIMOS-PASSOS.md, fase de sinalização."
    );
    std::process::exit(1);
}
