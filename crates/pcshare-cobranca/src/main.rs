//! **PCShare Cobrança** — webhook de pagamento.
//!
//! Recebe a notificação do Asaas quando uma cobrança é paga, **emite uma
//! licença** (reusa `pcshare_encontro::admin::emitir_licenca`, o mesmo arquivo
//! que o servidor de encontro lê para cobrar) e **envia a chave por e-mail**
//! pela API do Brevo. Roda no VPS, atrás do nginx (que faz o HTTPS e repassa
//! para a porta local deste serviço).
//!
//! Segurança, na ordem em que importa:
//! - **Token**: só aceita chamadas com o cabeçalho `asaas-access-token` igual ao
//!   `COBRANCA_TOKEN` (o mesmo que se cola na configuração do webhook no Asaas).
//! - **Valor**: só emite se o valor pago for o esperado (evita emitir por uma
//!   cobrança de outro produto/valor).
//! - **Idempotente**: o Asaas reenvia o evento; guardamos o id do pagamento já
//!   processado e não emitimos duas vezes.
//!
//! Rede de segurança: se a licença foi emitida mas o e-mail falhou, a venda vai
//! para `pendentes.txt` (chave + e-mail), para o operador reenviar à mão. Assim
//! um cliente nunca fica sem a chave sem o operador saber.
//!
//! Configuração por variáveis de ambiente (as 4 primeiras são obrigatórias):
//! ```text
//! COBRANCA_TOKEN      token combinado com o Asaas (cabeçalho do webhook)
//! PCSHARE_LICENCAS    arquivo de licenças (o mesmo do servidor)
//! ASAAS_API_KEY       chave da API do Asaas (para buscar o e-mail do comprador)
//! BREVO_API_KEY       chave da API do Brevo (para enviar o e-mail)
//! COBRANCA_BIND       endereço local de escuta        (padrão 127.0.0.1:7200)
//! ASAAS_API_BASE      base da API do Asaas    (padrão sandbox api-sandbox…/v3)
//! COBRANCA_VALOR      valor esperado da venda          (padrão 130.00)
//! COBRANCA_DIAS       validade da licença, em dias     (padrão 365)
//! EMAIL_DE            remetente          (padrão nao-responda@pcshare.com.br)
//! EMAIL_DE_NOME       nome do remetente                (padrão PCShare)
//! COBRANCA_PROCESSADOS  arquivo de ids já processados  (padrão processados.txt)
//! COBRANCA_PENDENTES    arquivo de e-mails que falharam (padrão pendentes.txt)
//! ```
//!
//! Teste do e-mail isolado (depois de verificar o domínio no Brevo):
//! `pcshare-cobranca teste-email fulano@exemplo.com`

use std::path::{Path, PathBuf};

use pcshare_encontro::admin::emitir_licenca;

struct Config {
    bind: String,
    token: String,
    licencas: PathBuf,
    asaas_base: String,
    asaas_key: String,
    brevo_key: String,
    valor: f64,
    dias: i64,
    email_de: String,
    email_de_nome: String,
    processados: PathBuf,
    pendentes: PathBuf,
}

impl Config {
    fn do_ambiente() -> Config {
        let obrig = |nome: &str| {
            std::env::var(nome).unwrap_or_else(|_| {
                eprintln!("cobranca: falta a variável obrigatória {nome}");
                std::process::exit(2);
            })
        };
        let opt = |nome: &str, padrao: &str| std::env::var(nome).unwrap_or_else(|_| padrao.into());
        Config {
            token: obrig("COBRANCA_TOKEN"),
            licencas: PathBuf::from(obrig("PCSHARE_LICENCAS")),
            asaas_key: obrig("ASAAS_API_KEY"),
            brevo_key: obrig("BREVO_API_KEY"),
            bind: opt("COBRANCA_BIND", "127.0.0.1:7200"),
            asaas_base: opt("ASAAS_API_BASE", "https://api-sandbox.asaas.com/v3"),
            valor: opt("COBRANCA_VALOR", "130.00").parse().unwrap_or(130.0),
            dias: opt("COBRANCA_DIAS", "365").parse().unwrap_or(365),
            email_de: opt("EMAIL_DE", "nao-responda@pcshare.com.br"),
            email_de_nome: opt("EMAIL_DE_NOME", "PCShare"),
            processados: PathBuf::from(opt("COBRANCA_PROCESSADOS", "processados.txt")),
            pendentes: PathBuf::from(opt("COBRANCA_PENDENTES", "pendentes.txt")),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Modo de teste do e-mail, isolado: `pcshare-cobranca teste-email <dest>`.
    if args.first().map(String::as_str) == Some("teste-email") {
        let cfg = Config::do_ambiente();
        let dest = args.get(1).cloned().unwrap_or_default();
        if dest.is_empty() {
            eprintln!("uso: pcshare-cobranca teste-email <destinatario>");
            std::process::exit(2);
        }
        match enviar_email(&cfg, &dest, "Teste", "TESTE-1234-TESTE-5678", "01/01/2030") {
            Ok(()) => println!("e-mail de teste enviado para {dest}"),
            Err(e) => {
                eprintln!("falha no envio: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let cfg = Config::do_ambiente();
    let servidor = match tiny_http::Server::http(&cfg.bind) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cobranca: não consegui escutar em {}: {e}", cfg.bind);
            std::process::exit(1);
        }
    };
    println!(
        "cobranca: ouvindo em {} | valor R$ {:.2} | licença {} dias | Asaas {}",
        cfg.bind, cfg.valor, cfg.dias, cfg.asaas_base
    );

    for mut req in servidor.incoming_requests() {
        let codigo = tratar(&mut req, &cfg);
        let _ = req.respond(tiny_http::Response::empty(codigo));
    }
}

/// Processa uma requisição e devolve o código HTTP a responder.
///
/// Convenção com o Asaas: **200** = tratado ou ignorado (não reenvie); **500** =
/// falha transitória antes de emitir (pode reenviar); **401** = token errado.
fn tratar(req: &mut tiny_http::Request, cfg: &Config) -> u16 {
    if *req.method() != tiny_http::Method::Post {
        return 405;
    }

    // 1) Token. Sem o cabeçalho certo, é chamada não autorizada.
    let token_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("asaas-access-token"))
        .map(|h| h.value.as_str() == cfg.token)
        .unwrap_or(false);
    if !token_ok {
        eprintln!("cobranca: chamada com token inválido — ignorada");
        return 401;
    }

    // 2) Corpo → JSON.
    let mut corpo = String::new();
    if std::io::Read::read_to_string(req.as_reader(), &mut corpo).is_err() {
        return 400;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&corpo) else {
        return 400;
    };

    let evento = v["event"].as_str().unwrap_or("");
    let pag_id = v["payment"]["id"].as_str().unwrap_or("");

    // 3) Só tratamos eventos de pagamento efetivado. Qualquer outro: ignora.
    if !matches!(evento, "PAYMENT_CONFIRMED" | "PAYMENT_RECEIVED") {
        return 200;
    }
    if pag_id.is_empty() {
        eprintln!("cobranca: evento {evento} sem id de pagamento — ignorado");
        return 200;
    }

    // 4) Idempotência: o Asaas reenvia. Se já emitimos por este pagamento, ok.
    if ja_processado(&cfg.processados, pag_id) {
        println!("cobranca: pagamento {pag_id} já processado — ok");
        return 200;
    }

    // 5) RECONFIRMA na fonte. Em vez de confiar no corpo do aviso, consulta o
    //    pagamento na API do Asaas e usa o status/valor/cliente que ELE informa.
    //    Assim, mesmo que o token vaze, ninguém emite licença sem um pagamento
    //    REAL e confirmado — um aviso forjado aponta para um id que não existe.
    let (status, valor, cliente_id) = match buscar_pagamento(cfg, pag_id) {
        Ok(x) => x,
        Err((404, _)) => {
            eprintln!("cobranca: pagamento {pag_id} não existe no Asaas — ignorado");
            return 200;
        }
        Err((_, msg)) => {
            eprintln!("cobranca: falha ao reconfirmar {pag_id}: {msg}");
            return 500; // transitório: o Asaas reenvia
        }
    };
    // RECEIVED (Pix/boleto pago), CONFIRMED (cartão) e RECEIVED_IN_CASH
    // (confirmado à mão pelo dono da conta — cobre venda paga por fora). Só o
    // dono da conta consegue marcar in cash, então não é brecha.
    if !matches!(status.as_str(), "CONFIRMED" | "RECEIVED" | "RECEIVED_IN_CASH") {
        println!("cobranca: pagamento {pag_id} com status {status} (não pago) — ignora");
        return 200;
    }
    if (valor - cfg.valor).abs() > 0.01 {
        eprintln!("cobranca: pagamento {pag_id} de R$ {valor:.2} ≠ R$ {:.2} — não emite", cfg.valor);
        return 200;
    }
    if cliente_id.is_empty() {
        eprintln!("cobranca: pagamento {pag_id} sem cliente — ignora");
        return 200;
    }

    // 6) E-mail do comprador (id vindo da fonte, reconfirmado).
    let (email, nome) = match buscar_cliente(cfg, &cliente_id) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cobranca: falha ao buscar o cliente {cliente_id}: {e}");
            return 500; // transitório, nada emitido: o Asaas pode reenviar
        }
    };

    // 7) Emite a licença.
    let nota = format!("{email} asaas:{pag_id}");
    let (chave, validade) = match emitir_licenca(&cfg.licencas, cfg.dias, &nota) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cobranca: falha ao emitir licença: {e}");
            return 500; // nada gravado, o Asaas pode reenviar
        }
    };
    // A partir daqui a chave EXISTE — marca processado para não emitir de novo.
    marcar(&cfg.processados, pag_id);
    println!("cobranca: emitida {chave} (val {validade}) para {email} [pag {pag_id}]");

    // 8) Envia por e-mail. Se falhar, a chave já existe — registra em pendentes
    //    para o operador reenviar à mão (não reemite via 500, senão duplicaria).
    if let Err(e) = enviar_email(cfg, &email, &nome, &chave, &validade) {
        eprintln!("cobranca: e-mail falhou para {email}: {e} — em pendentes");
        marcar(
            &cfg.pendentes,
            &format!("{pag_id} | {email} | {chave} | {validade} | ERRO: {e}"),
        );
    } else {
        println!("cobranca: chave enviada para {email}");
    }
    200
}

/// Reconfirma o pagamento direto na API do Asaas — a fonte da verdade. Devolve
/// `(status, valor, cliente_id)`. O erro carrega o código HTTP para o chamador
/// separar "não existe" (404 → ignora) de falha transitória (→ reenviar).
fn buscar_pagamento(cfg: &Config, pag_id: &str) -> Result<(String, f64, String), (u16, String)> {
    let url = format!("{}/payments/{pag_id}", cfg.asaas_base);
    let resp = match ureq::get(&url).set("access_token", &cfg.asaas_key).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, _)) => return Err((code, format!("HTTP {code}"))),
        Err(e) => return Err((0, e.to_string())),
    };
    let v: serde_json::Value = resp.into_json().map_err(|e| (0, e.to_string()))?;
    let status = v["status"].as_str().unwrap_or("").to_string();
    let valor = v["value"].as_f64().unwrap_or(0.0);
    let cliente = v["customer"].as_str().unwrap_or("").trim().to_string();
    Ok((status, valor, cliente))
}

/// Busca `(email, nome)` do cliente na API do Asaas.
fn buscar_cliente(cfg: &Config, cliente_id: &str) -> Result<(String, String), String> {
    let url = format!("{}/customers/{cliente_id}", cfg.asaas_base);
    let resp = ureq::get(&url)
        .set("access_token", &cfg.asaas_key)
        .call()
        .map_err(|e| e.to_string())?;
    let v: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;
    let email = v["email"].as_str().unwrap_or("").trim().to_string();
    let nome = v["name"].as_str().unwrap_or("").trim().to_string();
    if email.is_empty() {
        return Err("cliente sem e-mail cadastrado".into());
    }
    Ok((email, nome))
}

/// Envia a chave por e-mail pela API transacional do Brevo.
fn enviar_email(
    cfg: &Config,
    para_email: &str,
    para_nome: &str,
    chave: &str,
    validade: &str,
) -> Result<(), String> {
    let html = format!(
        "<div style=\"font-family:Arial,sans-serif;font-size:15px;color:#10203A\">\
         <h2 style=\"color:#0A1E3D\">Sua licença do PCShare</h2>\
         <p>Obrigado pela assinatura! Sua chave de licença é:</p>\
         <p style=\"font-size:22px;font-weight:bold;letter-spacing:2px;background:#F3F7FC;\
         border:1px solid #DBE6F3;border-radius:8px;padding:14px 18px;text-align:center\">{chave}</p>\
         <p>Válida até <b>{validade}</b>.</p>\
         <p><b>Como ativar, em 3 passos:</b></p>\
         <ol style=\"line-height:1.7\">\
         <li>Baixe o PCShare: <a href=\"https://www.pcshare.com.br/baixar/PCShare.exe\">pcshare.com.br/baixar/PCShare.exe</a><br>\
         <span style=\"color:#5A7291;font-size:13px\">Se o Windows avisar que o editor é desconhecido, clique em <b>Mais informações &rarr; Executar assim mesmo</b>.</span></li>\
         <li>Abra o PCShare e vá em <b>Configurações</b>.</li>\
         <li>Cole a chave acima, salve e conecte.</li>\
         </ol>\
         <p style=\"color:#5A7291;font-size:13px;margin-top:24px\">PCShare — acesso remoto por ID. \
         Este e-mail é automático; não responda. Dúvidas: contato@pcshare.com.br</p></div>"
    );
    let corpo = serde_json::json!({
        "sender": { "name": cfg.email_de_nome, "email": cfg.email_de },
        "to": [ { "email": para_email, "name": para_nome } ],
        "subject": "Sua licença do PCShare",
        "htmlContent": html,
    });
    ureq::post("https://api.brevo.com/v3/smtp/email")
        .set("api-key", &cfg.brevo_key)
        .set("accept", "application/json")
        .send_json(corpo)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Já emitimos por este id de pagamento?
fn ja_processado(caminho: &Path, id: &str) -> bool {
    std::fs::read_to_string(caminho)
        .map(|c| c.lines().any(|l| l.split(" | ").next().unwrap_or(l).trim() == id))
        .unwrap_or(false)
}

/// Anexa uma linha a um arquivo (idempotência / pendentes). Best-effort.
fn marcar(caminho: &Path, linha: &str) {
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(caminho) {
        Ok(mut f) => {
            let _ = writeln!(f, "{linha}");
        }
        Err(e) => eprintln!("cobranca: não consegui gravar em {}: {e}", caminho.display()),
    }
}
