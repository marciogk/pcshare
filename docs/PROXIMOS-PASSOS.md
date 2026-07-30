# Próximos passos — guia prático

O que depende de ação manual. Atualizado em 30/07/2026.

Só restam **duas** coisas. Tudo o mais já foi feito ou automatizado.

---

## 1. Desligar o Smart App Control  🔴 trava a próxima fase

**Por que:** o Windows avalia cada executável recém-compilado e às vezes o
bloqueia com *"Uma política de Controle de Aplicativo bloqueou este arquivo"*.
Já derrubou a suíte de testes duas vezes e impediu uma verificação.

Mais importante: a **Fase 3** (UAC e tela de bloqueio) exige instalar um
serviço do Windows. Um serviço não assinado não passa pelo Smart App Control.
Sem resolver isto, a próxima fase não começa.

**Antes de fazer, entenda:** desligar é **irreversível**. O Windows não permite
religar; só uma reinstalação limpa do sistema o traz de volta. Numa máquina de
desenvolvimento costuma valer a pena, mas a decisão é sua.

**Como:**

1. Tecla Windows → digite `Segurança do Windows` → Enter
2. Menu da esquerda → **Controle de aplicativo e navegador**
3. **Configurações de Controle Inteligente de Aplicativos**
4. Marque **Desativado** e confirme
5. **Reinicie o computador**

**Confirmar** — no PowerShell, deve responder `0`:

```powershell
(Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy").VerifiedAndReputablePolicyState
```

---

## 2. Etapa 5 — testar entre duas máquinas  🟢 precisa de um segundo PC

**Por que:** tudo até aqui rodou em `127.0.0.1`, que não tem rede de verdade —
sem perda de pacotes, sem limite de banda, sem firewall. É justamente onde os
defeitos de transporte não aparecem.

**O que você precisa:** um segundo PC com Windows na mesma rede.

### Na máquina que será controlada (host)

Descubra o IP **da placa de rede real** — ignore adaptadores de Hyper-V,
VirtualBox ou WSL:

```powershell
Get-NetIPAddress -AddressFamily IPv4 |
  Where-Object { $_.InterfaceAlias -notmatch 'vEthernet|WSL|Loopback' } |
  Select-Object InterfaceAlias, IPAddress
```

Copie a pasta `target\release\` para essa máquina e rode:

```powershell
.\pcshare-spike.exe host 0.0.0.0:7000
```

Ele imprime o **ID**, a **senha da sessão** e a **impressão digital**. Anote os
três.

Se o Windows perguntar sobre o firewall, **permita em redes privadas**. Se não
perguntar e a conexão falhar, num PowerShell **como administrador**:

```powershell
New-NetFirewallRule -DisplayName "PCShare spike" -Direction Inbound `
  -Protocol UDP -LocalPort 7000 -Action Allow -Profile Private
```

> Note: **UDP**, não TCP. O transporte agora é QUIC.

### Na máquina que vai controlar (viewer)

```powershell
.\pcshare-spike.exe viewer 192.168.0.15:7000 --password 482913
```

Troque pelo IP e pela senha exibidos no host. Confira se a impressão digital
que o viewer mostra é igual à do host — é isso que detecta intermediação.

> ⚠️ **Sem `--view-only`, o mouse e o teclado controlam a outra máquina de
> verdade.** É o comportamento pretendido, mas só faça numa máquina sua e à
> qual consiga voltar fisicamente.

### O que anotar

- A latência reportada fica estável ou volta a crescer?
- Aparece `desc` (descartes por contrapressão) maior que zero?
- Quantos Mbps na rede real?
- Algum erro de protocolo no terminal?

---

## Já resolvido — não precisa mais fazer

- ~~Conferir os `.h264` no VLC~~ — verificado gravando um quadro decodificado
  como imagem e conferindo diretamente.
- ~~Medir glass-to-glass com câmera~~ — o modo `latencia` estima sozinho,
  fechando o laço pela própria tela. Resultado: 53,6 ms medidos, **64 a 79 ms**
  estimados de glass-to-glass, dentro da meta de 80 ms.
  A foto com celular ainda serve se quiser um número oficial, mas hoje ela
  confirmaria uma estimativa em vez de produzir o primeiro dado.
- ~~Medir CPU~~ — 7,3% de um núcleo, 63 MB de RAM.

```powershell
# Medições que rodam sozinhas, quando quiser repetir
.\target\release\pcshare-spike.exe latencia 30
.\target\release\pcshare-spike.exe latencia-captura 40
```

---

## Decisão de negócio ainda em aberto

Duas, e nenhuma é técnica:

1. **O nicho.** Está indefinido desde o primeiro dia. Afeta que recursos
   priorizar depois da Fase 3.
2. **Certificado de assinatura de código.** Um **OV** basta para passar pelo
   Smart App Control, ou o **EV** é obrigatório? Quanto tempo leva para
   acumular reputação partindo do zero? Isso afeta preço e data de lançamento.
   Ver risco R1 em [ARQUITETURA.md](ARQUITETURA.md).
