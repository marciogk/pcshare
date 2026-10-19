#!/usr/bin/env bash
# Publica a landing do PCShare no VPS.
#
# Rode do **Git Bash** (é lá que moram o `ssh` e o `scp` nesta máquina — o
# PowerShell não os tem no PATH), a partir de qualquer pasta:
#
#     ./site/publicar.sh
#
# Faz, nesta ordem: recompila o Agente e o PCShare em release, monta o pacote em
# `site/baixar/`, envia a pasta `site/` inteira por scp, confere a configuração
# do nginx, recarrega e verifica o que subiu.
#
# Sobre o servidor: o VPS é gerenciado pelo painel iContainer e o **nginx roda
# em container** (`ic-nginx-hyFE`, rede host). Por isso o recarregamento é um
# `docker exec`, não um `systemctl` — não existe nginx de distro na máquina.
# Os server blocks ficam em conf.d/zz-pcshare.conf, escritos à mão: o painel não
# reescreve arquivo que não foi ele quem gerou. O "zz-" é de propósito (o
# cabeçalho do arquivo explica): mantém o bloco 443 do painel como padrão.
# O certificado se renova sozinho (certbot no host + gancho que copia para o
# container); este script não mexe nele.
#
# Dá para sobrescrever qualquer coisa por variável de ambiente:
#     VPS_USER=outro ./site/publicar.sh
set -euo pipefail

VPS_USER="${VPS_USER:-root}"
VPS_HOST="${VPS_HOST:-198.50.117.107}"
VPS_RAIZ="${VPS_RAIZ:-/etc/icontainer/apps/nginx/nginx/www/sites/pcshare.com.br/index}"
NGINX_CT="${NGINX_CT:-ic-nginx-hyFE}"
SSH_CHAVE="${SSH_CHAVE:-$HOME/.ssh/retifica_vps}"
DOMINIO="${DOMINIO:-www.pcshare.com.br}"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

SSH_ARGS=(-o BatchMode=yes)
[ -n "$SSH_CHAVE" ] && SSH_ARGS+=(-i "$SSH_CHAVE")
alvo="$VPS_USER@$VPS_HOST"

echo "==> 1/5  recompilando o Agente e o PCShare em release"
cargo build --release -p pcshare-agente -p pcshare-ui

echo "==> 2/5  montando site/baixar/"
mkdir -p site/baixar
cp target/release/pcshare-agente.exe site/baixar/PCShare-Agente.exe
cp target/release/pcshare.exe site/baixar/PCShare.exe
sha_local="$(sha256sum site/baixar/PCShare-Agente.exe | cut -d' ' -f1)"
bytes_local="$(stat -c%s site/baixar/PCShare-Agente.exe)"
echo "    PCShare-Agente.exe  $bytes_local bytes  sha256 ${sha_local:0:16}…"
sha_local_ui="$(sha256sum site/baixar/PCShare.exe | cut -d' ' -f1)"
bytes_local_ui="$(stat -c%s site/baixar/PCShare.exe)"
echo "    PCShare.exe         $bytes_local_ui bytes  sha256 ${sha_local_ui:0:16}…"

echo "==> 3/5  enviando para $alvo:$VPS_RAIZ"
# scp não tem --exclude; monto uma cópia sem este script e mando ela
tmp="$(mktemp -d)"
cp -r site/. "$tmp/"
rm -f "$tmp/publicar.sh"
scp "${SSH_ARGS[@]}" -q -r "$tmp/." "$alvo:$VPS_RAIZ/"
rm -rf "$tmp"

echo "==> 4/5  conferindo e recarregando o nginx"
ssh "${SSH_ARGS[@]}" "$alvo" \
    "docker exec $NGINX_CT nginx -t && docker exec $NGINX_CT nginx -s reload && echo '    nginx recarregado'"

echo "==> 5/5  verificando o que está no ar"
# Enquanto o DNS não apontar para o VPS, bate no IP fingindo ser o domínio.
# A checagem procura o IP na resposta em vez de olhar o código de saída: o
# nslookup do Windows devolve 0 mesmo quando o domínio não existe.
if nslookup "$DOMINIO" 2>/dev/null | grep -q "$VPS_HOST"; then
    BASE="https://$DOMINIO"
    CURL=(curl -s)
else
    echo "    (o DNS de $DOMINIO ainda não resolve — verificando pelo IP)"
    BASE="http://$VPS_HOST"
    CURL=(curl -s -H "Host: $DOMINIO")
fi

code="$("${CURL[@]}" -o /dev/null -w '%{http_code}' "$BASE/")"
tipo="$("${CURL[@]}" -I "$BASE/" | tr -d '\r' | awk -F': ' 'tolower($1)=="content-type"{print $2}')"
echo "    $BASE/  → HTTP $code   ($tipo)"

sha_remoto="$("${CURL[@]}" "$BASE/baixar/PCShare-Agente.exe" | sha256sum | cut -d' ' -f1)"
if [ "$sha_remoto" = "$sha_local" ]; then
    echo "    download confere: sha256 idêntico ao binário local"
else
    echo "    !! o .exe baixado NÃO confere com o local"
    echo "       local:  $sha_local"
    echo "       remoto: $sha_remoto"
    exit 1
fi
sha_remoto_ui="$("${CURL[@]}" "$BASE/baixar/PCShare.exe" | sha256sum | cut -d' ' -f1)"
if [ "$sha_remoto_ui" = "$sha_local_ui" ]; then
    echo "    PCShare.exe confere: sha256 idêntico ao binário local"
else
    echo "    !! o PCShare.exe baixado NÃO confere com o local"
    exit 1
fi

echo
echo "pronto: https://$DOMINIO"
