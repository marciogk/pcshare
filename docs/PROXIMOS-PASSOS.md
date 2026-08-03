# Próximos passos

Atualizado em 03/08/2026, ao encerrar.

---

## 🔴 PRIMEIRO DE TUDO: o desenvolvimento está bloqueado

O Smart App Control está **ligado** e impede compilar em release, rodar testes
e executar qualquer binário. Só compilar em debug ainda funciona, o que não
permite verificar nada.

A mensagem do Windows nomeia a causa: *"não é possível confirmar quem publicou
`pcshare.exe`"* — falta de assinatura digital. Ver a captura em
[ARQUITETURA.md](ARQUITETURA.md), risco R1.

**Desligar antes de qualquer outra coisa:**

```
Tecla Windows → "Segurança do Windows" → Enter
→ Controle de aplicativo e navegador
→ Configurações de Controle Inteligente de Aplicativos
→ Desativado
```

Conferir (tem de responder `0`):

```powershell
(Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy").VerifiedAndReputablePolicyState
```

Depois, reconstruir o que a limpeza apagou:

```powershell
cd "c:\Projeto Pcshare"
cargo build --release --workspace
cargo test --workspace
```

> **Lição registrada:** foi um `cargo clean --release` que disparou isso. A
> limpeza invalidou artefatos que já tinham reputação, e cada script de
> compilação virou executável de hash inédito. Com o Smart App Control ligado,
> **não limpar o perfil release**.

> Vale manter esta máquina como a única que reproduz a experiência real do
> cliente: religar o Smart App Control depois, para testar o instalador
> assinado quando houver certificado.

---

## Estado anterior (30/07/2026)

**O Sprint 0 está completo.** Todos os critérios foram atingidos, incluindo a
etapa 5 entre duas máquinas em rede real. Os resultados estão em
[SPRINT-0.md](SPRINT-0.md).

---

## Como retomar os testes

Nada precisa ser reconfigurado. O que já está feito e **permanece**:

| Item | Estado |
|---|---|
| Rede `Ethernet` como Privada | ✅ persiste entre reinícios |
| Regra de firewall `PCShare spike` (UDP 7000) | ✅ persiste |
| `pcshare.exe` no notebook | ✅ em `C:\Pcshare\` |
| Identidade das duas máquinas | ✅ em `%LOCALAPPDATA%\PCShare\` |

O único cuidado: **o Smart App Control está ligado** neste PC. Binários já
executados passam; um recém-compilado pode ser bloqueado.

### Retomar em três comandos

**1. Neste PC (host):**

```powershell
cd "c:\Projeto Pcshare"
.\target\release\pcshare-spike.exe host 0.0.0.0:7000 --view-only --password 482913
```

**2. Confirme o IP** — pode ter mudado por DHCP:

```powershell
(Get-NetIPAddress -AddressFamily IPv4 | Where-Object InterfaceAlias -eq 'Ethernet').IPAddress
```

**3. No notebook, em `C:\Pcshare\`:**

```powershell
.\pcshare.exe viewer 192.168.15.3:7000 --password 482913 --view-only
```

> **Uma sessão por vez.** O host aceita uma conexão e para de escutar. Para
> reconectar, encerre o host e inicie de novo. Se der `timed out`, quase sempre
> é isto — não é problema de rede.

### Verificações rápidas, se algo falhar

```powershell
# o host está escutando?
Get-NetUDPEndpoint -LocalPort 7000

# a regra de firewall continua lá?
Get-NetFirewallRule -DisplayName "PCShare spike" | Select-Object Enabled, Action

# a rede ainda é Privada?
Get-NetConnectionProfile | Select-Object InterfaceAlias, NetworkCategory
```

---

## O que ficou pendente

### 1. Confirmar a origem das duas impressões digitais 🟡

As duas sessões reportaram identidades diferentes para o que se supunha ser a
mesma máquina. O código está descartado como causa — o teste de recarga da
identidade passa e o arquivo foi confirmado no disco.

Para confirmar, **no notebook**:

```powershell
.\pcshare.exe host 0.0.0.0:7001 --password 1
```

Ele imprime o ID e a impressão digital **dele mesmo**; depois `Ctrl+C`.

- Se for `8D:62:14:DD:B8:58:C7:DC` → a sessão longa veio de outra máquina
  (provavelmente a de `192.168.0.249`), e não há defeito.
- Se for `B4:84:8F:1D:8D:E9:5F:45` → há algo a investigar.

### 2. Teste de controle remoto real 🟢

Tudo foi testado com `--view-only`, que impede a injeção de mouse e teclado.
Falta rodar **sem** essa opção nas duas pontas, para ver o controle funcionando.

> Só faça numa máquina à qual consiga voltar fisicamente.

### 3. ~~Reconexão automática~~ ✅ corrigido em 31/07/2026

O host agora atende sessões indefinidamente e sobrevive a senha errada. O log
distingue os desfechos:

```
--- sessão 1 encerrada: viewer desconectou ---
--- sessão 2 encerrada: autenticação recusada (senha incorreta) ---
--- sessão 3 encerrada: viewer desconectou ---
```

**Consequência prática para os testes:** não é mais preciso reiniciar o host
entre tentativas. Se der `timed out` agora, é rede de verdade — não a vaga
única de sessão.

---

## Fase 3 — já começou

**Feito e commitado** (`crates/pcshare-capture/src/desktop.rs`): detecção de
qual desktop recebe entrada (`Default`, `Winlogon`, `Screen-saver`) e de qual
sessão o processo ocupa. Um processo comum recebe **acesso negado** ao
consultar o desktop de entrada enquanto o Secure Desktop está ativo, e essa
negativa é o sinal, não um erro. Nada disso exige privilégio elevado.

**Próximo passo, assim que der para rodar código:**

1. Instrumentar a captura com o detector e **medir a falha** — quanto tempo
   fica cega durante um prompt de UAC, com que sintoma, se `recover()` resolve.
   Exige acionar um prompt de UAC com o medidor rodando.
2. Só então desenhar o serviço + agente da decisão D5, com o problema
   quantificado em vez de suposto.

A instalação do serviço exigirá elevação e, quase certamente, o Smart App
Control desligado — um serviço não assinado dificilmente passa.

---

## Decisões de negócio em aberto

1. **O nicho de mercado.** Indefinido desde o primeiro dia. Afeta que recursos
   priorizar depois da Fase 3.
2. **Certificado de assinatura.** Um **OV** basta para passar pelo Smart App
   Control, ou o **EV** é obrigatório? Quanto tempo leva para acumular
   reputação partindo do zero? Afeta preço e data de lançamento. Ver risco R1
   em [ARQUITETURA.md](ARQUITETURA.md).
