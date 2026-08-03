# Próximos passos

Atualizado em 30/07/2026, ao encerrar a sessão de testes.

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

## Próximo marco técnico

**Fase 3 — arquitetura serviço + agente**, para funcionar durante prompts de
UAC e na tela de bloqueio. É o maior risco de cronograma restante e o que
separa um protótipo de um produto de suporte remoto.

Exige elevação de privilégio e, muito provavelmente, desligar o Smart App
Control aqui — um serviço não assinado dificilmente passa.

---

## Decisões de negócio em aberto

1. **O nicho de mercado.** Indefinido desde o primeiro dia. Afeta que recursos
   priorizar depois da Fase 3.
2. **Certificado de assinatura.** Um **OV** basta para passar pelo Smart App
   Control, ou o **EV** é obrigatório? Quanto tempo leva para acumular
   reputação partindo do zero? Afeta preço e data de lançamento. Ver risco R1
   em [ARQUITETURA.md](ARQUITETURA.md).
