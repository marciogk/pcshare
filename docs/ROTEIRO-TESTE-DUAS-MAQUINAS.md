# Roteiro do teste entre duas máquinas

Documento autocontido para quem vai conduzir o teste. Não exige conhecer o
resto do projeto.

**Duração estimada:** 30 a 45 minutos, incluindo um reinício.

---

## Contexto em cinco linhas

O PCShare é um software de acesso remoto (estilo AnyDesk) escrito em Rust,
ainda em fase de protótipo. A pipeline completa já funciona: captura de tela
por DXGI, codificação H.264 por hardware, sessão cifrada com autenticação por
ID e senha, transporte QUIC, decodificação e injeção de mouse e teclado.

**Tudo foi validado até hoje rodando em `127.0.0.1`** — mesma máquina, sem rede
real. Este teste é a primeira execução entre dois computadores de verdade.

## O que se quer descobrir

Loopback não tem perda de pacote, limite de banda nem firewall. É exatamente
onde defeitos de transporte **não** aparecem. As perguntas:

1. A conexão se estabelece através da rede e do firewall?
2. A autenticação por ID e senha funciona entre máquinas distintas?
3. A latência se mantém estável, ou volta a crescer sem limite?
4. Aparecem descartes por falta de banda?
5. Surge algum erro de protocolo?

---

## Pré-requisitos

### 1. Smart App Control — **nas duas máquinas**

Windows 11 recente bloqueia executáveis sem assinatura nem reputação, de forma
**intermitente**. Já derrubou a suíte de testes deste projeto duas vezes.

Verificar (PowerShell, qualquer usuário):

```powershell
(Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy").VerifiedAndReputablePolicyState
```

- `0` — desligado, pode seguir
- `1` — **ligado**, vai atrapalhar
- chave ausente — recurso não presente nessa versão, pode seguir

Para desligar: Segurança do Windows → Controle de aplicativo e navegador →
Configurações de Controle Inteligente de Aplicativos → Desativado → **reiniciar**.

> ⚠️ A Microsoft documenta o desligamento como **definitivo**. Na prática, em
> 30/07/2026 nesta build 26200, foi possível religar pela mesma interface sem
> reinstalar nem reiniciar. Trate como possivelmente irreversível.

**Antes de desligar, tente sem.** Binários já executados antes costumam ganhar
reputação local e passar mesmo com o SAC ligado. O bloqueio atinge sobretudo
executáveis recém-compilados, com hash inédito.

### 2. Rede e firewall — na máquina que será o **host**

O perfil de rede precisa ser **Privado**. Em perfil Público o firewall bloqueia
praticamente tudo, e abrir porta num perfil marcado como público é a postura
errada.

PowerShell **como administrador**:

```powershell
# Confira o nome da interface antes; costuma ser "Ethernet" ou "Wi-Fi"
Get-NetConnectionProfile

Set-NetConnectionProfile -InterfaceAlias "Ethernet" -NetworkCategory Private

New-NetFirewallRule -DisplayName "PCShare spike" -Direction Inbound `
  -Protocol UDP -LocalPort 7000 -Action Allow -Profile Private
```

> **É UDP, não TCP.** O transporte é QUIC, que roda sobre UDP.

### 3. Descobrir o IP real do host

Ignore adaptadores virtuais de Hyper-V, WSL e VirtualBox:

```powershell
Get-NetIPAddress -AddressFamily IPv4 |
  Where-Object { $_.InterfaceAlias -notmatch 'vEthernet|WSL|Loopback' -and $_.IPAddress -notlike '127.*' } |
  Select-Object InterfaceAlias, IPAddress
```

No PC de mesa deste projeto, o IP era `192.168.15.3` — confirme, pode ter
mudado por DHCP.

---

## O binário

`pacote-notebook\pcshare.exe` é **autocontido**: 3,1 MB, runtime C ligado
estaticamente, não exige o redistribuível do Visual C++ nem qualquer
instalação. Copie o arquivo e rode.

As duas máquinas usam o mesmo executável; o que muda é o argumento.

---

## Execução

### Na máquina que será controlada (host)

```powershell
.\pcshare.exe host 0.0.0.0:7000 --view-only
```

Ele imprime:

```
  ID desta máquina : 226 796 386
  Senha da sessão  : 482913
  Impressão digital: 54:34:16:6E:5C:5E:45:62
```

Anote a **senha** e a **impressão digital**.

### Na máquina que vai controlar (viewer)

```powershell
.\pcshare.exe viewer 192.168.15.3:7000 --password 482913 --view-only
```

Troque o IP e a senha. Deve abrir uma janela mostrando a tela do host.

### Sobre o `--view-only`

Impede que o mouse e o teclado do viewer controlem a outra máquina.

**Mantenha nas duas pontas na primeira tentativa.** Depois que a imagem
aparecer, repita **sem** o `--view-only` nas duas para testar o controle real —
mas só numa máquina à qual se consiga voltar fisicamente.

---

## O que coletar

Copie a saída completa do terminal **das duas máquinas**. As linhas relevantes:

**No host**, a cada 2 s:

```
host: 495 frames | leitura GPU 0.00 ms | BGRA→NV12 0.08 ms | encode 3.93 ms
      | 0.58 Mbps | fila 113 KB | ritmo 681 desc 0 sem-mudanca 0
```

- `fila` crescendo sem parar → o link não escoa
- `desc` maior que zero → descarte por contrapressão (**é o dado mais
  importante deste teste**; em loopback sempre foi zero)

**No viewer**, a cada 2 s:

```
viewer: 344 exibidos, 5 obsoletos, 0 eventos | RTT p50 6.9 ms  p99 8.9 ms
```

- RTT estável ou crescendo?
- `obsoletos` subindo muito → o viewer não acompanha

**Conferir também:** a impressão digital exibida pelo viewer tem de ser
**idêntica** à do host. Divergência indica intermediação na rede.

**Qualquer linha com `mensagem inválida`, `erro`, `panicked`** — copiar
integralmente. Erros de protocolo são o achado mais valioso possível aqui.

---

## Se não conectar

| Sintoma | Causa provável |
|---|---|
| `connect falhou` / timeout | firewall UDP 7000, ou perfil de rede Público |
| `Uma política de Controle de Aplicativo bloqueou este arquivo` | Smart App Control ligado nessa máquina |
| `não foi possível autenticar` | senha digitada errada |
| conecta e cai logo depois | anotar a mensagem exata — é achado real |

Teste de alcance básico, do viewer para o host:

```powershell
Test-NetConnection 192.168.15.3 -Port 7000 -InformationLevel Detailed
```

> `Test-NetConnection` testa **TCP**, e nosso serviço é UDP — então ele vai
> falhar mesmo com tudo certo. Serve apenas para confirmar que as máquinas se
> enxergam na rede (o campo `PingSucceeded`).

---

## Contexto extra, se interessar

- Arquitetura e decisões técnicas: [ARQUITETURA.md](ARQUITETURA.md)
- Critérios e resultados de desempenho: [SPRINT-0.md](SPRINT-0.md)

Números de referência obtidos em loopback, para comparação:
latência tela→exibição **53,6 ms**, banda **0,5 a 0,9 Mbps**, CPU **7,3%** de
um núcleo, **zero** descartes.
