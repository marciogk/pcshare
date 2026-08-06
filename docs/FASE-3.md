# Fase 3 — UAC e tela de bloqueio

O que separa um protótipo de um produto de suporte remoto.

---

## Passo 1 — a cegueira, medida

Medido em 05/08/2026, host Windows 11 (i7-4790, HD 4600, 1360×768), com a
ferramenta `uacwatch` (`crates/pcshare-capture/src/bin/uacwatch.rs`).

Duas execuções, prompt de UAC acionado pela própria ferramenta:

| | Execução 1 | Execução 2 |
|---|---|---|
| Secure Desktop ativo | 3804 ms | 5342 ms |
| **Janela de cegueira** | **3950 ms** | **5347 ms** |
| Custo da recuperação | 146 ms | 5 ms |
| `recover()` falhou | 75× | 105× |
| Recuperou na tentativa | 76 | 106 |

### O que ficou provado

**Um processo comum não consegue duplicar o Secure Desktop.** Todas as 180
tentativas somadas falharam com `0x80070005` — acesso negado — enquanto o
desktop `Winlogon` esteve à frente. Assim que o desktop do usuário voltou, a
recuperação funcionou na primeira tentativa.

Isso valida a decisão D5 com evidência. A arquitetura serviço + agente não é
preferência de projeto: é a única saída.

**A recuperação em si não é o problema.** Custa entre 5 e 146 ms, e depois
disso o primeiro quadro chega em menos de 20 ms. A estratégia de insistir está
correta e já está implementada.

### O que o número esconde, e é pior

A cegueira dura **exatamente o tempo que o prompt fica na tela**. A diferença
entre as duas execuções — 3,8 s contra 5,3 s — é só quanto o operador demorou
para clicar.

Numa sessão de suporte real, isso não é uma pausa de quatro segundos. É uma
pausa **indefinida**:

- o técnico não vê o prompt, então não sabe o que apareceu;
- não consegue clicar, porque a injeção de entrada também não alcança o
  Secure Desktop;
- só quem está fisicamente na máquina pode resolver — e essa pessoa pediu
  ajuda justamente por não saber o que fazer.

O atendimento trava até alguém do outro lado agir às cegas, por telefone. É o
pior momento possível para perder a conexão, e é exatamente onde ela se perde
hoje.

---

## Defeitos que a medição encontrou

Nenhum deles teria aparecido sem rodar o experimento.

### 1. `recover()` falhando inutilizava a captura

Quando `duplicate_output` falhava, o `?` retornava cedo e `self.duplication`
seguia apontando para o objeto morto. A chamada seguinte quebrava com
`INVALID_CALL` (`0x887A0001`) — erro que nada diz sobre a causa e que abortou
a primeira medição.

A duplicação virou `Option`, solta antes da tentativa de recriar. Falhar deixa
o estado honesto: sem duplicação. `recover()` passou a ser idempotente.

### 2. O host derrubava a sessão em todo prompt de UAC

A linha era `cap.recover()?`. Como `recover()` falha durante o Secure Desktop,
o erro subia e encerrava a sessão inteira.

Em produção: o cliente clica em algo que pede elevação e o técnico é
desconectado — no momento em que mais precisaria estar conectado.

O host agora segue cego, insistindo em segundo plano, e continua atendendo o
transporte para que pings e eventos de entrada não parem junto com a imagem.

### 3. O laço girava em falso

Sem duplicação válida, `next_frame` responde na hora. Medido em ~2000
tentativas por segundo, queimando um núcleo durante cada prompt. Entrou uma
pausa de 50 ms entre tentativas.

Exceção deliberada à decisão D8, que proíbe esperar por relógio: aqui não há
evento a aguardar, porque o Windows não avisa quando sai do Secure Desktop.

### 4. A primeira métrica estava errada

A versão inicial reportou "janela de cegueira: 3 ms" — número que não
descrevia nada. Ela marcava o fim da cegueira no primeiro quadro que chegasse,
mas um quadro entregue **durante** o prompt é o desktop do usuário congelado,
não a tela que a pessoa está vendo.

A cegueira só termina depois que o Secure Desktop sai.

---

## Passo 2 — o mecanismo de lançamento, provado

Medido em 05/08/2026 com `pcshare-service experimento`, sem privilégio
elevado.

O supervisor lança um agente preso a um desktop, via
`STARTUPINFO.lpDesktop`, e o agente relata se consegue capturar dali.

| Caso | Desktop do agente | É o de entrada? | Captura |
|---|---|---|---|
| 1 | `Default` | sim | **funcionou** — 195 quadros em 3 s |
| 2 | `PcshareExperimento` | não | **falhou** — `0x80070005` |

O caso 2 é o controle negativo, e vale tanto quanto o positivo: sem ele, o
caso 1 não distinguiria "a amarração funciona" de "a amarração é irrelevante".

### O que isso muda no entendimento

**Não há nada de especial no `Winlogon`.** O erro do caso 2 é idêntico ao
medido durante o prompt de UAC — mesmo `0x80070005`, num desktop comum criado
por nós. A regra é geral:

> Um processo só duplica a tela quando está preso ao desktop que está
> recebendo entrada.

O Secure Desktop não é uma exceção com proteção própria; é só mais um desktop
que não é o nosso. Isso simplifica o desenho: a solução não precisa de nenhum
tratamento especial para UAC, precisa de **um agente por desktop de entrada**.

### O custo do relançamento

| | |
|---|---|
| Primeiro lançamento (disco frio) | 617,7 ms |
| Segundo lançamento (quente) | **7,5 ms** |

Em produção o binário já estará quente. **7,5 ms contra 4 a 5 segundos de
cegueira** — ordens de grandeza de folga. O relançamento não vai ser o gargalo.

---

## Passo 3 — supervisor e serviço, implementados

`pcshare-service` ganhou três modos além do experimento:

| Comando | O que faz | Precisa de admin? |
|---|---|---|
| `supervisor` | o laço completo, como usuário comum | não |
| `instalar` | registra o serviço (início **manual**) | sim |
| `desinstalar` | remove o serviço | sim |
| *(sem argumento)* | entrega o processo ao Gerenciador de Serviços | — |

O laço observa o desktop de entrada a cada 50 ms e, quando ele muda, mata o
agente e lança outro amarrado ao desktop novo. Não há como reconfigurar um
processo já rodando: ele nasce preso a um desktop.

Verificado em modo console: um agente, vivo, **598 quadros em 10 s** (~60 fps),
sem relançamentos espúrios.

### Dois defeitos que o primeiro teste do supervisor revelou

**1. O agente morria ao nascer.** Lançado com `CREATE_NO_WINDOW`, ficava sem
console, e o primeiro `println!` entrava em pânico — em Rust, `println!` aborta
quando a escrita falha. O supervisor relançava a cada 100 ms sem perceber.

Um processo lançado por serviço não pode depender de console. O agente passou a
registrar em `C:\ProgramData\PCShare\agente-<desktop>.log`, ignorando falha de
escrita no console.

**2. O log confundia os dois motivos de relançamento.** A mensagem dizia
"desktop mudou: Default -> Default", que não faz sentido e escondeu o defeito
acima. Relançar porque o desktop mudou é funcionamento normal; relançar porque
o agente morreu é sintoma. Agora aparecem separados.

### O que só a instalação vai responder

- O `Winlogon` aceita `CreateProcessAsUserW` com `lpDesktop`? Ele tem
  descritor de segurança próprio, e é o último ponto onde ainda cabe surpresa.
- `WTSQueryUserToken` funciona da sessão 0 nesta máquina?
- Quanto tempo o supervisor leva para **perceber** a troca — hoje o limite
  superior é o intervalo de consulta, 50 ms, contra os 4 a 5 segundos de
  cegueira atuais.

---

## Passo 4 — o que vem agora

Com o problema quantificado, o desenho do serviço + agente pode começar:

```
pcshare-service.exe    → Windows Service, sessão 0, SYSTEM
        │                 sobrevive a logoff, alcança o Winlogon
        │ WTSQueryUserToken + CreateProcessAsUser
        ▼
pcshare-agent.exe      → um por desktop ativo
                         captura, encode, injeta entrada
                         relançado quando o desktop troca
```

A linha de base contra a qual comparar já existe: **cegueira igual à duração
do prompt**. A solução funciona se esse número cair para perto de zero.

O custo do relançamento já foi medido — 7,5 ms com o binário quente — e não
será o gargalo.

O que resta desconhecido, e só se resolve com o serviço instalado:

- o serviço consegue de fato abrir e amarrar um agente ao `Winlogon`? A
  mecânica está provada para desktops comuns, mas o `Winlogon` tem descritor
  de segurança próprio;
- quanto tempo o supervisor leva para **perceber** a troca de desktop. Hoje a
  detecção é por consulta em laço; se for lenta, ela vira o novo gargalo, não
  o lançamento.

**Bloqueio conhecido:** instalar um serviço exige elevação, e um serviço não
assinado dificilmente passa pelo Smart App Control. Ver risco R1 em
[ARQUITETURA.md](ARQUITETURA.md).

---

## Como repetir a medição

```powershell
cargo run --release -p pcshare-capture --bin uacwatch
```

Aciona o prompt sozinho; basta clicar em **Não**. Nada é executado com
elevação — `cmd /c exit` não faz nada nem se for aceito por engano.

Para a tela de bloqueio (Win+L) ou Ctrl+Alt+Del, que não dão para acionar por
programa:

```powershell
cargo run --release -p pcshare-capture --bin uacwatch -- --manual
```
