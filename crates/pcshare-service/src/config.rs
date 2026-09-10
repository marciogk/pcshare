//! Configuração do supervisor.
//!
//! # Por que um arquivo, e não variáveis de ambiente
//!
//! Porque com serviço elas não funcionam, e a falha é silenciosa. Um serviço
//! do Windows recebe o ambiente do **sistema**, não o do console de quem
//! chamou `Start-Service` — então definir `$env:PCSHARE_SENHA` antes de iniciar
//! o serviço não tem efeito nenhum.
//!
//! Isso aconteceu de verdade, em 17/08/2026: o serviço subiu com senha
//! sorteada e controle remoto habilitado, apesar de as duas variáveis terem
//! sido definidas. Ninguém conseguiu conectar, e o teste que importava não foi
//! feito. Um caminho de configuração que falha sem avisar é pior que não ter
//! configuração.
//!
//! O ambiente **continua valendo e tem prioridade**, porque em modo console
//! ele é o jeito mais direto. O arquivo é o que funciona nos dois.
//!
//! Formato, uma chave por linha; `#` comenta:
//!
//! ```text
//! senha=482913
//! bind=0.0.0.0:7000
//! somente_ver=1
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use crate::supervisor::log;

pub struct Config {
    valores: HashMap<String, String>,
    /// De onde veio, para o log dizer.
    pub origem: String,
}

impl Config {
    pub fn carregar() -> Self {
        let caminho = caminho();
        match std::fs::read_to_string(&caminho) {
            Ok(texto) => Self {
                valores: interpretar(&texto),
                origem: caminho.display().to_string(),
            },
            Err(_) => Self {
                valores: HashMap::new(),
                origem: format!("{} (ausente)", caminho.display()),
            },
        }
    }

    /// Ambiente primeiro, arquivo depois. Quem roda em console costuma querer
    /// mudar uma coisa só, sem editar arquivo.
    fn buscar(&self, chave: &str) -> Option<String> {
        let ambiente = format!("PCSHARE_{}", chave.to_uppercase());
        std::env::var(&ambiente)
            .ok()
            .or_else(|| self.valores.get(chave).cloned())
    }

    pub fn senha(&self) -> Option<String> {
        self.buscar("senha")
    }

    pub fn bind(&self) -> Option<String> {
        self.buscar("bind")
    }

    /// `somente_ver=1` liga; `0`, vazio ou ausente desliga.
    pub fn somente_ver(&self) -> bool {
        self.buscar("somente_ver")
            .is_some_and(|v| !v.is_empty() && v != "0")
    }

    /// Endereço do servidor de encontro, se houver.
    ///
    /// Com ele, o host **conecta de saída** e se registra sob o próprio ID, em
    /// vez de escutar numa porta — que ninguém alcança atrás de um NAT. É o que
    /// permite conectar pela internet digitando o ID, como o AnyDesk. Sem ele,
    /// o comportamento é o de sempre: escutar em `bind`, para LAN e tailnet.
    pub fn encontro(&self) -> Option<std::net::SocketAddr> {
        let bruto = self.buscar("encontro")?;
        match bruto.parse() {
            Ok(a) => Some(a),
            Err(e) => {
                log(&format!("encontro inválido ('{bruto}'): {e}; ignorando"));
                None
            }
        }
    }

    /// Permite que o viewer envie Ctrl+Alt+Del. **Desligado por padrão.**
    ///
    /// Fica desligado porque depende de uma política do Windows,
    /// `SoftwareSASGeneration`, que numa instalação limpa costuma estar em `0`
    /// — e nesse valor o `SendSAS` é recusado. Medido em 08/09/2026: recusado
    /// em `0`, funciona a partir de `1`.
    ///
    /// A escolha de produto é não elevar essa chave na máquina de ninguém sem
    /// que o administrador peça. Quem precisa de destravamento remoto liga as
    /// duas coisas; quem não precisa não tem a postura de segurança alterada.
    pub fn ctrl_alt_del(&self) -> bool {
        self.buscar("ctrl_alt_del")
            .is_some_and(|v| !v.is_empty() && v != "0")
    }

    /// Relata o que foi lido, **sem a senha**.
    pub fn relatar(&self) {
        log(&format!("configuração: {}", self.origem));
        if !self.valores.is_empty() {
            let mut chaves: Vec<&str> = self.valores.keys().map(String::as_str).collect();
            chaves.sort_unstable();
            log(&format!("configuração: chaves lidas — {}", chaves.join(", ")));
        }
    }
}

pub fn caminho() -> PathBuf {
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("config.txt")
}

fn interpretar(texto: &str) -> HashMap<String, String> {
    texto
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreta_pares_ignorando_comentarios_e_espaco() {
        let c = interpretar(
            "# comentário\n  senha = 482913 \n\nbind=0.0.0.0:7000\nSOMENTE_VER=1\nlixo sem igual\n",
        );
        assert_eq!(c.get("senha").map(String::as_str), Some("482913"));
        assert_eq!(c.get("bind").map(String::as_str), Some("0.0.0.0:7000"));
        // Chave em maiúscula tem de casar com a busca, que usa minúscula.
        assert_eq!(c.get("somente_ver").map(String::as_str), Some("1"));
        assert_eq!(c.len(), 3, "linha sem '=' não vira chave");
    }

    #[test]
    fn senha_com_igual_dentro_sobrevive() {
        // `split_once` divide no primeiro `=`, então o resto fica intacto.
        let c = interpretar("senha=a=b=c\n");
        assert_eq!(c.get("senha").map(String::as_str), Some("a=b=c"));
    }
}
