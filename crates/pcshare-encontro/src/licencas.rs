//! Store de licenças: quem pode **acessar** (o operador que assina).
//!
//! MVP de emissão manual: um arquivo texto no servidor, **uma licença por
//! linha**, relido a cada checagem — assim uma edição à mão (você acrescenta a
//! chave quando alguém paga) vale na hora, sem reiniciar o serviço.
//!
//! Formato da linha: `chave;validade;status`
//! - `chave`   — a chave que o operador digita no app.
//! - `validade`— `AAAA-MM-DD` (fim da assinatura); vazio = sem prazo.
//! - `status`  — `ativa` para valer; qualquer outra coisa bloqueia.
//!
//! Linhas em branco e as que começam com `#` são ignoradas (comentários).
//!
//! O host (quem é acessado) **não** passa por aqui: só o viewer apresenta
//! licença. Sem arquivo configurado, o servidor roda em **modo aberto** e nem
//! chama isto.

use std::path::PathBuf;

/// Resultado de conferir uma chave.
#[derive(Debug, PartialEq, Eq)]
pub enum Veredito {
    Ok,
    Invalida,
    Expirada,
}

/// Store lido de um arquivo. Barato de clonar (guarda só o caminho); relê o
/// disco a cada `conferir`.
#[derive(Clone)]
pub struct Licencas {
    caminho: PathBuf,
}

impl Licencas {
    pub fn do_arquivo(caminho: impl Into<PathBuf>) -> Self {
        Self {
            caminho: caminho.into(),
        }
    }

    /// Confere uma chave contra o arquivo. Sem arquivo legível, ninguém entra
    /// (`Invalida`) — um servidor em cobrança que perdeu o arquivo deve fechar,
    /// não abrir.
    pub fn conferir(&self, chave: &str) -> Veredito {
        let chave = chave.trim();
        if chave.is_empty() {
            return Veredito::Invalida;
        }
        let conteudo = match std::fs::read_to_string(&self.caminho) {
            Ok(c) => c,
            Err(_) => return Veredito::Invalida,
        };
        for linha in conteudo.lines() {
            let linha = linha.trim();
            if linha.is_empty() || linha.starts_with('#') {
                continue;
            }
            let mut campos = linha.split(';');
            let k = campos.next().unwrap_or("").trim();
            if k != chave {
                continue;
            }
            let validade = campos.next().unwrap_or("").trim();
            let status = campos.next().unwrap_or("ativa").trim();
            if !status.eq_ignore_ascii_case("ativa") {
                return Veredito::Invalida;
            }
            return match prazo_dias(validade) {
                // Sem prazo declarado: vale sempre.
                None => Veredito::Ok,
                // Vence no fim do dia da validade: expira só quando o dia de
                // hoje passa dele.
                Some(dia) if dia < hoje_em_dias() => Veredito::Expirada,
                Some(_) => Veredito::Ok,
            };
        }
        Veredito::Invalida
    }
}

/// Dia de hoje (UTC) em dias desde 1970-01-01.
fn hoje_em_dias() -> i64 {
    let segundos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    segundos.div_euclid(86_400)
}

/// `AAAA-MM-DD` → dias desde a época. `None` se vazio; data malformada vira uma
/// validade já vencida (retorna `Some(i64::MIN)`) para o conservador prevalecer.
fn prazo_dias(validade: &str) -> Option<i64> {
    if validade.is_empty() {
        return None;
    }
    let mut p = validade.split('-');
    let y = p.next().and_then(|s| s.parse::<i64>().ok());
    let m = p.next().and_then(|s| s.parse::<i64>().ok());
    let d = p.next().and_then(|s| s.parse::<i64>().ok());
    match (y, m, d) {
        (Some(y), Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            Some(dias_da_data(y, m, d))
        }
        _ => Some(i64::MIN),
    }
}

/// Dias desde 1970-01-01 para uma data do calendário (algoritmo de Howard
/// Hinnant, `days_from_civil`). Evita depender de um crate de datas.
fn dias_da_data(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn arquivo_com(conteudo: &str) -> (tempfile_like::Temp, Licencas) {
        let t = tempfile_like::Temp::novo();
        let mut f = std::fs::File::create(&t.caminho).unwrap();
        f.write_all(conteudo.as_bytes()).unwrap();
        let l = Licencas::do_arquivo(t.caminho.clone());
        (t, l)
    }

    #[test]
    fn chave_ativa_e_no_prazo_passa() {
        let (_t, l) = arquivo_com("ABC-123;2999-12-31;ativa\n");
        assert_eq!(l.conferir("ABC-123"), Veredito::Ok);
    }

    #[test]
    fn sem_prazo_vale_sempre() {
        let (_t, l) = arquivo_com("PERP-1;;ativa\n");
        assert_eq!(l.conferir("PERP-1"), Veredito::Ok);
    }

    #[test]
    fn vencida_expira() {
        let (_t, l) = arquivo_com("OLD-1;2000-01-01;ativa\n");
        assert_eq!(l.conferir("OLD-1"), Veredito::Expirada);
    }

    #[test]
    fn desativada_bloqueia() {
        let (_t, l) = arquivo_com("X-1;2999-01-01;suspensa\n");
        assert_eq!(l.conferir("X-1"), Veredito::Invalida);
    }

    #[test]
    fn desconhecida_e_vazia_sao_invalidas() {
        let (_t, l) = arquivo_com("# comentario\nA-1;2999-01-01;ativa\n");
        assert_eq!(l.conferir("NAO-EXISTE"), Veredito::Invalida);
        assert_eq!(l.conferir("   "), Veredito::Invalida);
    }

    #[test]
    fn data_conhecida_bate() {
        // 1970-01-01 é o dia 0; 1970-01-02 é o dia 1.
        assert_eq!(dias_da_data(1970, 1, 1), 0);
        assert_eq!(dias_da_data(1970, 1, 2), 1);
        assert_eq!(dias_da_data(2000, 1, 1), 10_957);
    }

    /// Arquivo temporário mínimo, só para os testes, sem puxar um crate.
    mod tempfile_like {
        pub struct Temp {
            pub caminho: std::path::PathBuf,
        }
        impl Temp {
            pub fn novo() -> Self {
                let nome = format!(
                    "pcshare-lic-teste-{}-{}.txt",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                );
                Self {
                    caminho: std::env::temp_dir().join(nome),
                }
            }
        }
        impl Drop for Temp {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.caminho);
            }
        }
    }
}
