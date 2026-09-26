//! Integração com o Windows para o acesso não assistido: guardar a senha fixa
//! **cifrada** (DPAPI), ligar/desligar o início com o Windows (chave Run do
//! usuário) e impedir duas instâncias do Agente (mutex nomeado).
//!
//! Nada aqui exige privilégio de administrador: a chave Run é a do usuário
//! (`HKEY_CURRENT_USER`) e o DPAPI cifra atado à conta de quem está logado.

use std::path::Path;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::{PCWSTR, w};

/// Nome do valor na chave Run e do arquivo da senha.
const RUN_VALOR: &str = "PCShareAgente";
const CHAVE_RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

// ---------------------------------------------------------------- instância única

/// `true` se este é o **primeiro** Agente rodando nesta sessão. Impede que o
/// Agente auto-iniciado e um aberto à mão registrem o mesmo ID em dobro (a
/// armadilha "senha não confere" de dois hosts). O handle vaza de propósito: o
/// mutex precisa viver enquanto o processo viver.
pub fn instancia_unica() -> bool {
    unsafe {
        match CreateMutexW(None, true, w!("PCShareAgenteInstanciaUnica")) {
            // Sucesso mas já existia → não somos o primeiro.
            Ok(_) => windows::Win32::Foundation::GetLastError() != ERROR_ALREADY_EXISTS,
            // Não deu para criar: não é motivo para impedir o app de abrir.
            Err(_) => true,
        }
    }
}

// ---------------------------------------------------------------- senha (DPAPI)

fn blob(dados: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: dados.len() as u32,
        pbData: dados.as_ptr() as *mut u8,
    }
}

/// Copia o resultado do DPAPI e libera o buffer que ele alocou (`LocalAlloc`).
unsafe fn coletar(saida: &CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let v = std::slice::from_raw_parts(saida.pbData, saida.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(saida.pbData as *mut _)));
        v
    }
}

fn proteger(claro: &[u8]) -> Option<Vec<u8>> {
    let entrada = blob(claro);
    let mut saida = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &entrada,
            PCWSTR::null(),
            None,
            None,
            None,
            0,
            &mut saida,
        )
        .ok()?;
        Some(coletar(&saida))
    }
}

fn desproteger(cifrado: &[u8]) -> Option<Vec<u8>> {
    let entrada = blob(cifrado);
    let mut saida = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &entrada,
            None,
            None,
            None,
            None,
            0,
            &mut saida,
        )
        .ok()?;
        Some(coletar(&saida))
    }
}

/// Cifra a senha (DPAPI) e a grava em `caminho`. Nunca em texto claro: é a
/// credencial de acesso permanente da máquina.
pub fn salvar_senha(caminho: &Path, senha: &str) -> bool {
    match proteger(senha.as_bytes()) {
        Some(cifrada) => std::fs::write(caminho, cifrada).is_ok(),
        None => false,
    }
}

/// Lê e decifra a senha guardada, se houver.
pub fn ler_senha(caminho: &Path) -> Option<String> {
    let cifrada = std::fs::read(caminho).ok()?;
    let claro = desproteger(&cifrada)?;
    String::from_utf8(claro).ok()
}

/// Apaga a senha guardada (ao desligar o modo não assistido).
pub fn apagar_senha(caminho: &Path) {
    let _ = std::fs::remove_file(caminho);
}

// ---------------------------------------------------------------- início automático

/// Liga ou desliga o início do Agente com o Windows, escrevendo/removendo o
/// valor na chave Run do usuário. Devolve `true` se a operação valeu.
pub fn definir_inicio_automatico(ligar: bool) -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run = match hkcu.create_subkey(CHAVE_RUN) {
        Ok((chave, _)) => chave,
        Err(_) => return false,
    };
    if ligar {
        // Entre aspas porque o caminho tem espaço ("C:\...\Projeto Pcshare\...").
        run.set_value(RUN_VALOR, &format!("\"{}\"", exe.display()))
            .is_ok()
    } else {
        // Remover algo que não existe não é erro para o nosso propósito.
        let _ = run.delete_value(RUN_VALOR);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{desproteger, proteger};

    #[test]
    fn dpapi_cifra_e_decifra_de_volta() {
        let claro = b"k7$Pq2mZ-uma-senha-forte";
        let cifrado = proteger(claro).expect("proteger");
        assert_ne!(cifrado.as_slice(), claro, "o blob tem de sair cifrado");
        let volta = desproteger(&cifrado).expect("desproteger");
        assert_eq!(volta.as_slice(), claro, "o ida-e-volta preserva a senha");
    }
}
