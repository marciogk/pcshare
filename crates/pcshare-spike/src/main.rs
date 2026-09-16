//! Casca de linha de comando do PCShare.
//!
//! Todo o motor — captura, codec, transporte, sessão do host e do viewer,
//! transferência de arquivos e os modos de medição — vive em `pcshare-sessao`.
//! Este binário só repassa os argumentos, para os testes entre máquinas e as
//! medições continuarem existindo enquanto a interface não cobre tudo.

fn main() {
    pcshare_sessao::run(std::env::args().skip(1).collect());
}
