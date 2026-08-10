//! Canal do agente para o supervisor.
//!
//! # Por que ele passou a existir
//!
//! Até aqui o agente relatava a troca de desktop **encerrando**, com o
//! destino no código de saída. Funciona, mas obriga o supervisor a criar um
//! processo novo no caminho crítico — e a medição mostrou que criar o agente
//! com token de usuário custa cerca de 700 ms, contra 120 ms com token de
//! SYSTEM. Era 90% da cegueira ao voltar do UAC.
//!
//! Com um canal, o agente relata **sem morrer**. Os agentes passam a viver por
//! desktop: o do `Default` fica cego enquanto o UAC está na frente e volta
//! sozinho quando ele sai. O relançamento sai do caminho crítico.
//!
//! # Sobre a permissão do cano
//!
//! O supervisor cria o cano como SYSTEM; os agentes do desktop do usuário
//! rodam como usuário. Sem um descritor de segurança explícito, eles não
//! conseguiriam abrir — foi exatamente assim que a linha do tempo perdeu
//! metade dos eventos em silêncio, e a lição foi cara o bastante para não se
//! repetir aqui.

use std::sync::mpsc::Sender;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_WRITE, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows::core::{PCWSTR, PWSTR};

const NOME: &str = r"\\.\pipe\pcshare-supervisor";

/// Concede leitura e escrita a todos.
///
/// `WD` é "Everyone". Amplo de propósito e aceitável **porque o canal só
/// carrega o nome do desktop ativo** — informação que qualquer processo local
/// já obtém sozinho com `OpenInputDesktop`. Não passa quadro, senha nem
/// entrada. Se algum dia passar, este descritor precisa ser estreitado para o
/// usuário da sessão.
const SDDL: &str = "D:(A;;GRGW;;;WD)";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Envia uma linha ao supervisor. `false` se ele não estiver ouvindo.
#[must_use]
pub fn enviar(msg: &str) -> bool {
    enviar_para(NOME, msg)
}

/// Idem, com o cano nomeado explicitamente.
///
/// Existe para os testes: eles rodam em paralelo, e um cano fixo faria o teste
/// que exige "nenhum servidor no ar" encontrar o servidor que o outro subiu.
#[must_use]
pub fn enviar_para(cano: &str, msg: &str) -> bool {
    let nome = wide(cano);
    // Poucas tentativas curtas: todas as instâncias podem estar ocupadas no
    // instante exato do envio. Insistir para sempre travaria a captura, que é
    // o trabalho de verdade do agente.
    for tentativa in 0..3 {
        unsafe {
            if let Ok(h) = CreateFileW(
                PCWSTR(nome.as_ptr()),
                FILE_GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            ) {
                let mut escritos = 0u32;
                let ok = WriteFile(h, Some(msg.as_bytes()), Some(&mut escritos), None).is_ok();
                let _ = CloseHandle(h);
                return ok;
            }
        }
        if tentativa < 2 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    false
}

/// Sobe uma thread que recebe as mensagens dos agentes.
///
/// Atende um cliente por vez, e isso basta: o canal carrega uma linha curta a
/// cada troca de desktop, não um fluxo.
pub fn servir(tx: Sender<String>) {
    servir_em(NOME, tx);
}

/// Idem, com o cano nomeado explicitamente. Ver [`enviar_para`].
pub fn servir_em(cano: &str, tx: Sender<String>) {
    let cano = cano.to_string();
    std::thread::Builder::new()
        .name("pcshare-canal".into())
        .spawn(move || {
            let Some(mut atual) = criar_instancia(&cano) else {
                // Sem cano não há canal; sair é melhor que girar em falso.
                return;
            };
            loop {
                unsafe {
                    // Erro aqui costuma ser cliente que conectou antes do
                    // `ConnectNamedPipe`, e nesse caso já há dados para ler.
                    let _ = ConnectNamedPipe(atual, None);

                    // A próxima instância nasce **agora**, com o cliente atual
                    // ainda sendo atendido. Criá-la depois de fechar esta
                    // deixava um intervalo sem cano algum, e o envio seguinte
                    // falhava — observado no segundo envio de um teste.
                    let proxima = criar_instancia(&cano);

                    let mut buf = [0u8; 512];
                    let mut lidos = 0u32;
                    let recebida =
                        if ReadFile(atual, Some(&mut buf), Some(&mut lidos), None).is_ok()
                            && lidos > 0
                        {
                            Some(
                                String::from_utf8_lossy(&buf[..lidos as usize])
                                    .trim()
                                    .to_string(),
                            )
                        } else {
                            None
                        };

                    let _ = DisconnectNamedPipe(atual);
                    let _ = CloseHandle(atual);

                    if let Some(msg) = recebida
                        && !msg.is_empty()
                        && tx.send(msg).is_err()
                    {
                        if let Some(p) = proxima {
                            let _ = CloseHandle(p);
                        }
                        return; // supervisor sumiu
                    }

                    match proxima {
                        Some(p) => atual = p,
                        None => return,
                    }
                }
            }
        })
        .ok();
}

fn criar_instancia(cano: &str) -> Option<HANDLE> {
    let nome = wide(cano);
    let sddl = wide(SDDL);
    unsafe {
        let mut descritor = PSECURITY_DESCRIPTOR::default();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descritor,
            None,
        )
        .is_err()
        {
            return None;
        }

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descritor.0,
            bInheritHandle: false.into(),
        };

        let h = CreateNamedPipeW(
            PCWSTR(nome.as_ptr()),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            8,
            0,
            512,
            0,
            Some(&sa),
        );
        if h.is_invalid() { None } else { Some(h) }
    }
}

/// Mensagem que o agente envia ao perceber a troca.
pub fn mensagem_de_desktop(nome: &str) -> String {
    format!("desktop {nome}")
}

/// Extrai o nome do desktop de uma mensagem, se for desse tipo.
pub fn desktop_da_mensagem(msg: &str) -> Option<&str> {
    msg.strip_prefix("desktop ").map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mensagem_de_desktop_e_reversivel() {
        let m = mensagem_de_desktop("Winlogon");
        assert_eq!(desktop_da_mensagem(&m), Some("Winlogon"));
    }

    #[test]
    fn mensagem_de_outro_tipo_e_ignorada() {
        assert_eq!(desktop_da_mensagem("qualquer coisa"), None);
        assert_eq!(desktop_da_mensagem(""), None);
    }

    /// Sem servidor no ar, enviar precisa falhar em vez de travar — o agente
    /// não pode parar de capturar porque o supervisor não está ouvindo.
    #[test]
    fn enviar_sem_servidor_falha_rapido() {
        let inicio = std::time::Instant::now();
        let ok = enviar_para(r"\\.\pipe\pcshare-teste-sem-servidor", "desktop Teste");
        assert!(!ok, "não deveria haver servidor neste cano");
        assert!(
            inicio.elapsed() < std::time::Duration::from_secs(2),
            "falhou, mas demorou demais"
        );
    }

    /// **Duas** mensagens seguidas, de propósito. A primeira versão criava a
    /// instância seguinte do cano só depois de fechar a atual, e o segundo
    /// envio caía num intervalo sem cano algum.
    #[test]
    fn ida_e_volta_pelo_cano() {
        let cano = r"\\.\pipe\pcshare-teste-ida-e-volta";
        let (tx, rx) = std::sync::mpsc::channel();
        servir_em(cano, tx);
        // Dá tempo de a instância do cano existir antes do primeiro envio.
        std::thread::sleep(std::time::Duration::from_millis(200));

        for esperado in ["Winlogon", "Default", "Screen-saver"] {
            assert!(
                enviar_para(cano, &mensagem_de_desktop(esperado)),
                "envio de {esperado} falhou"
            );
            let recebida = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{esperado} não chegou"));
            assert_eq!(desktop_da_mensagem(&recebida), Some(esperado));
        }
    }
}
