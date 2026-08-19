# Próximos passos

Atualizado em 17/08/2026, ao encerrar.

---

## Onde paramos (13/08/2026)

**A pipeline foi migrada para o agente.** A arquitetura passou a ser a que a
decisão D5 previa:

| Processo | O que faz | Por quê |
|---|---|---|
| **agente** | captura, converte na GPU, codifica, injeta entrada | só ele está preso ao desktop |
| **supervisor** | QUIC, Noise, repasse, contrapressão | só ele sobrevive à troca de desktop |

O canal entre os dois virou **bidirecional e persistente**: vídeo sobe, entrada
desce. 38 testes passando em release, contra 35 antes.

### O que foi verificado, e o que não

Subi o supervisor em modo console e conferi pelos logs:

```
[19:53:50] sessão: escutando em 0.0.0.0:7000
[19:53:50] sessão: ID desta máquina 895 570 225
[19:53:50] agente lançado em 'Default' — 12,1 ms desde a percepção
[Default] captura ABRIU 1360x768
[Default] encoder [Intel® Quick Sync Video H.264 Encoder MFT] (VBR, ...)
[Default] caminho de pixels = GPU direto (sem cópia)
[Default] vivo em 'Default' | 0 quadros | 0 perdas     ← 12 vezes, 25 s
```

Ou seja: a sessão sobe, o agente nasce, conecta ao canal, abre a captura e cria
o encoder por hardware. E **0 quadros** sem viewer conectado, que é o
comportamento correto — se fossem mais que zero, o agente estaria a caminho de
congelar.

*(Escrito em 13/08, quando nenhum quadro tinha atravessado ainda. Em 17/08 a
imagem foi confirmada em loopback e entre as duas máquinas — ver o item 1 de
"O que vem a seguir".)*

### Três defeitos encontrados durante a migração

Vale registrar, porque nenhum dos três estava previsto no plano:

1. **Descritor síncrono trava canal duplex.** O Windows serializa toda a E/S
   sobre um descritor síncrono, mesmo em direções opostas: a thread leitora
   parada em `ReadFile` bloqueava a escrita que traria os dados. Deadlock com
   0% de CPU. A saída foi `FILE_FLAG_OVERLAPPED` nas duas pontas.

2. **Invólucro `Send` contornado pela captura precisa de closures.** Havia um
   `struct Cano(HANDLE)` com `unsafe impl Send`, e o fechamento fazia
   `cano.0` — o que faz o Rust capturar o **campo**, não a struct, levando o
   `HANDLE` cru para a outra thread sem passar pelo `Send` do invólucro. O
   descritor agora atravessa como `usize`, que é `Send` de verdade.

3. **Agente congela sem viewer.** O agente empurrava vídeo mesmo sem ninguém
   assistindo; o cano enche em torno de meio segundo e a escrita seguinte
   bloqueia para sempre. Junto com o agente parariam os relatos de troca de
   desktop — a Fase 3 inteira. Resolvido com `Mensagem::Capturar`, que o
   supervisor liga quando um viewer chega.

### A permissão do cano foi estreitada — e não bastava

O plano previa restringir o cano ao usuário do console mais SYSTEM. Ao
escrever, ficou claro que **isso não fecha a brecha que o próprio plano
descreve**: o agente do desktop do usuário roda com o token do usuário, logo o
usuário precisa ter acesso ao cano — e passaria a poder ler os quadros do
`Winlogon`, que ele não consegue capturar sozinho.

Quem fecha é a verificação de PID: o supervisor só conversa com processos que
ele mesmo lançou. Para não haver corrida, o agente **nasce suspenso**, entra na
lista de autorizados, e só então é liberado.

---

## Estado anterior (10/08/2026)

**A Fase 3 está resolvida no essencial.** A arquitetura serviço + agente
funciona, e a cegueira durante um prompt de UAC caiu de **4600 ms para 91 ms**
— 50 vezes. Resultados e caminho completo em [FASE-3.md](FASE-3.md).

### Ambiente, como ficou

| | |
|---|---|
| Smart App Control | **desligado** (`0`) — desenvolvimento desimpedido |
| Serviço `PCShareSupervisor` | instalado, **parado**, início manual |
| Processos soltos | nenhum |
| Testes | 35, todos passando |

Para religar o serviço quando voltar, num PowerShell **como administrador**:

```powershell
Start-Service PCShareSupervisor
# ... provocar prompts de UAC ...
Stop-Service PCShareSupervisor
cd "c:\Projeto Pcshare"
.\target\release\pcshare-service.exe analisar
```

> **Lição registrada:** com o Smart App Control ligado, **nunca rodar
> `cargo clean --release`**. A limpeza invalida artefatos que já tinham
> reputação, e cada script de compilação vira executável de hash inédito — o
> que travou o desenvolvimento por completo em 03/08.

> Vale religar o Smart App Control quando houver certificado, para testar o
> instalador assinado. Esta é a única máquina que reproduz a experiência do
> cliente.

---

## O que vem a seguir

### 1. Medir a arquitetura nova 🟡 imagem e latência feitas; falta o UAC

**Imagem atravessa.** Supervisor, agente e viewer em loopback: `1360x768`,
decodificada, gravada em BMP e conferida a olho — é a tela de verdade, texto
legível. O caminho `captura → GPU → encode → cano → QUIC → Noise → decode`
está inteiro de pé.

**Latência: a divisão em três processos não custou nada mensurável.**

| | tela → captura | captura → decodificação | total p50 |
|---|---|---|---|
| spike, processo único | 43,4 ms | 10,2 ms | **53,6 ms** |
| arquitetura nova | 39,5 a 47,9 ms | 8,5 a 11,2 ms | **50,4 / 50,7 / 59,0 ms** |

Três execuções de 40 amostras, p99 entre 66,7 e 68,2 ms. Medido com
`pcshare-spike latencia-sessao`, criado para isto — o modo `latencia` antigo
embute o host no próprio processo e mede a arquitetura que deixou de existir.

> **Duas armadilhas custaram medições falsas aqui, ambas corrigidas.**
>
> A primeira: `kill %1` não funciona em shell não interativo. Sobraram
> supervisores de execuções anteriores, cada um com seu agente capturando a
> tela, e as medições seguintes saíram com três pipelines disputando o encoder.
> O p99 foi de 66 ms para 520 ms **sem que nada no número denunciasse a
> causa**. O script agora encerra por nome e confere quantos processos existem
> antes de medir.
>
> A segunda: o viewer casa carimbo com quadro **por ordem de fila**, e todo
> quadro que entra no decodificador sem sair desalinha a fila para sempre. A
> decomposição derivou de 10 ms para 60 ms ao longo de cinco execuções, e as
> primeiras passaram por medida boa. Há agora um teto na fila e uma conferência
> antes de imprimir: se o trecho não couber dentro do total, o programa diz que
> não sabe em vez de inventar.

**As duas máquinas funcionam.** Em 17/08 o notebook exibiu a tela deste PC
pela arquitetura nova, numa sessão real. O log da última sessão:

```
sessão: 600 quadros repassados, 0 descartados, 104 eventos de entrada (NÃO injetados)
supervisor encerrado (0 trocas de desktop)
```

Zero descartes, e o `somente_ver=1` recebendo a entrada do viewer sem injetar.
O contador de quadros ficando parado por três relatórios e depois disparando é
a supressão de tela ociosa trabalhando.

### A sessão sobrevive ao Secure Desktop ✅ 18/08/2026

**Confirmado.** Quatro prompts de UAC com o notebook conectado, e nenhuma
linha de `sessão encerrada` entre eles:

```
[19:18:46] --- sessão 1 iniciada ---
[19:19:27] agente relatou: desktop de entrada é 'Winlogon'
[19:19:37] agente relatou: desktop de entrada é 'Winlogon'
[19:19:50] agente relatou: desktop de entrada é 'Winlogon'
[19:20:02] agente relatou: desktop de entrada é 'Winlogon'
[19:20:46] sessão: 707 quadros repassados, 0 descartados, 2 agentes no canal
[19:20:50] parada solicitada pelo Windows
```

A sessão durou os dois minutos inteiros, atravessando quatro trocas de
desktop, com **zero descartes**. É a justificativa da arquitetura inteira,
verificada.

Cegueira, oito janelas: mediana **92,6 ms**, contra os 91 ms de 10/08. A
migração não mexeu no regime permanente. A primeira janela deu 584,5 ms — o
agente do `Winlogon` nascendo com o encoder, que é o item 2.

> **Mas o viewer não viu o Secure Desktop.** O agente do `Winlogon` terminou a
> sessão com `0 quadros`: ele **capturou e nunca transmitiu**. `Capturar(true)`
> era difundido uma única vez, quando o viewer chega, e quem nasce depois
> nascia em espera.
>
> Ou seja, os 92 ms mediam **captura**, não entrega. Na tela do notebook o
> congelamento durou o prompt inteiro, um a dois segundos.
>
> Corrigido: o supervisor agora manda `Capturar(true)` e `PedirKeyframe` ao
> receber o `Pronto` de qualquer agente durante uma sessão. **Falta remedir com
> isso valendo** — e é a próxima coisa a fazer.

> **O viewer do notebook não precisa ser atualizado.** `proto.rs` não muda
> desde 29/07 e `quic`/`secure` desde 30/07; o binário lá é posterior. Tudo o
> que mudou foi o lado do host.

> **`--view-only` no viewer não protege nada.** É uma chave do lado do host no
> `pcshare-spike`; o viewer manda entrada de qualquer jeito. Quem protege a
> máquina é a máquina: `somente_ver=1` na configuração do supervisor. Sem
> isso, quem conectar move o mouse e digita de verdade — e o supervisor avisa
> na subida, em vez de deixar alguém descobrir depois.

> **`Start-Service` não enxerga `$env:`.** Um serviço do Windows recebe o
> ambiente do **sistema**, não o do console de quem o iniciou. Definir
> `$env:PCSHARE_SENHA` antes de `Start-Service` não tem efeito nenhum — e em
> 17/08 isso custou uma sessão inteira de teste: o serviço subiu com senha
> sorteada e controle habilitado, ninguém conseguiu conectar, e o teste que
> importava não foi feito.
>
> Por isso a configuração vive em **`C:\ProgramData\PCShare\config.txt`**:
>
> ```text
> senha=482913
> bind=0.0.0.0:7000
> somente_ver=1
> ```
>
> Vale nos dois modos. Variáveis de ambiente continuam funcionando e têm
> prioridade, o que é prático em modo console. Quando a senha não vem da
> configuração, o log diz que foi sorteada **e por quê**.

#### Como rodar o que falta

**a) Uma imagem qualquer, em modo console.** Num PowerShell **como
administrador**:

```powershell
cd "c:\Projeto Pcshare"
$env:PCSHARE_SENHA = "482913"
.\target\release\pcshare-service.exe supervisor
```

O log (`%ProgramData%\PCShare\supervisor.log`) deve mostrar o ID da máquina, a
porta, e `N agentes transmitindo` quando o viewer conectar. No notebook:

```powershell
.\pcshare.exe viewer 192.168.15.3:7000 --password 482913 --view-only
```

> Confirme o IP antes — pode ter mudado por DHCP:
> `(Get-NetIPAddress -AddressFamily IPv4 | Where-Object InterfaceAlias -eq 'Ethernet').IPAddress`

**b) O teste que justifica a arquitetura inteira.** Com o serviço instalado e o
viewer conectado, provocar um prompt de UAC. O esperado: a imagem congela por
volta de 90 ms e volta, **e a sessão não cai**. Se cair, a migração desfez a
Fase 3 e é ali que está o problema.

> **Provoque três ou quatro prompts, não um.** A primeira janela mede o
> nascimento do agente do `Winlogon` — hoje cerca de 550 ms, dos quais 362 ms
> são o encoder H.264 sendo montado. Da segunda em diante o agente já existe, e
> o número cai para a faixa de 80 ms. As duas coisas são reais; medir uma vez
> só não distingue qual você está vendo.
>
> O log agora contém **apenas a execução atual** — a anterior fica em
> `supervisor-anterior.log`. Pode ler do começo.

**c) Latência e banda,** contra os números do `pcshare-spike`: **53,6 ms**
tela→exibição e **0,5–0,9 Mbps**. O canal acrescentou uma travessia de
processo; se a latência subir muito, é ali.

> **`cargo test --release` não gera os executáveis.** Ele compila os binários
> de teste, não `target\release\pcshare-agent.exe`. Rodei o supervisor achando
> que testava o código novo e estava executando o de três dias antes — os
> sintomas eram coerentes o bastante para enganar. **Sempre `cargo build
> --release` antes de qualquer teste manual**, e na dúvida confira a data:
> `ls -l target/release/pcshare-agent.exe`.

> **Atenção ao instrumento.** A medição de latência do spike só valia com host
> e viewer no mesmo processo, porque comparava dois `Instant` da mesma origem.
> Agora são três processos. O agente carimba em relógio de parede, mas o viewer
> ainda subtrai do relógio dele — **medir sem arrumar isso dá número errado, e
> errado para baixo**, que é o pior caso. Três medições deste projeto já saíram
> plausíveis e falsas.

### 2. Tirar o encoder do caminho crítico 🟡 tentado e revertido

A primeira janela de cegueira custa ~557 ms contra ~78 ms das seguintes, e
362 ms disso é o encoder H.264 nascendo junto com o agente do `Winlogon`, que
só é criado no primeiro prompt de UAC.

**Pré-lançar o agente do `Winlogon` foi tentado em 17/08 e reverteu-se**, por
duas razões que se somaram num laço de seis processos por segundo:

1. O agente **encerra** quando a captura falha ao nascer, e um agente do
   `Winlogon` criado enquanto o `Default` está à frente não duplica nada. Ele
   morria no berço, sempre.
2. O recuo exponencial tinha sido excluído do aquecimento "para não atrasar o
   caminho crítico" — o que deixou justamente o agente que morre em ciclo sem
   espera nenhuma, enquanto as mortes dele incrementavam o contador que atrasa
   o agente de verdade.

Para retomar, **nesta ordem**:

1. O agente precisa **sobreviver a nascer cego**, entrando no laço de
   recuperação em vez de encerrar. Ele já sabe ficar cego no meio da vida; só
   o nascimento é assimétrico.
2. O encoder precisa ser montado sem depender da captura para saber as
   dimensões — `IDXGIOutput::GetDesc` responde de qualquer desktop, sem
   duplicação. Sem isso o agente pré-lançado ainda pagaria os 362 ms no
   momento do UAC, e o aquecimento não serviria para nada.
3. Só então pré-lançar, **com o recuo valendo para todos**.

### 3. Tela de bloqueio e Ctrl+Alt+Del 🟢

Mesma mecânica já provada, episódios mais longos. Baixo risco.

### 4. Reduzir os 90 ms restantes 🟡 retorno pequeno

Quase todos são a pausa de 50 ms entre tentativas de recuperar a captura no
agente ocioso. Baixá-la reduziria quase na mesma proporção, mas com três ou
quatro agentes vivos a conta de CPU muda. Medir antes de mexer.

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

## Fase 3 — passo 1 concluído

**A cegueira foi medida.** Resultados, defeitos encontrados e o desenho do
próximo passo estão em [FASE-3.md](FASE-3.md).

Em resumo: um processo comum não consegue duplicar o Secure Desktop (180
tentativas, todas com acesso negado), e a cegueira dura exatamente o tempo que
o prompt fica na tela. A recuperação em si custa menos de 150 ms.

Três defeitos sérios vieram junto, todos corrigidos — o pior deles derrubava a
sessão inteira em cada prompt de UAC.

## Fase 3 — histórico do início

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
