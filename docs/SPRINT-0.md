# Sprint 0 — Spike de latência

**Duração alvo:** 2 a 4 semanas
**Objetivo:** provar ou refutar o risco técnico número 1 antes de investir meses.

---

## O que este spike é

Uma pipeline mínima de ponta a ponta, na rede local:

```
DXGI Desktop Duplication  (com dirty rects)
        ▼
Encoder H.264 por hardware  (Media Foundation / Quick Sync)
        ▼
Transport::send  ──── TCP na LAN ────►  Transport::recv
                                              ▼
                                        Decoder H.264
                                              ▼
                                        Janela do viewer
                                              ▼
                                   eventos de mouse/teclado
                                              │
        ◄─────────────────────────────────────┘
                    SendInput no host
```

## O que este spike NÃO é

Deliberadamente fora de escopo — incluir qualquer um destes atrasa a resposta
à única pergunta que importa agora:

- ❌ Criptografia
- ❌ Contas, login, IDs
- ❌ NAT traversal / WebRTC / servidor de sinalização
- ❌ Windows Service, UAC, tela de bloqueio
- ❌ Instalador
- ❌ Interface bonita
- ❌ Multi-monitor
- ❌ Transferência de arquivos, clipboard, áudio

---

## Restrição da máquina de desenvolvimento

O monitor de desenvolvimento é **1360×768** (resolução nativa, confirmada) e a
GPU é uma Intel HD Graphics 4600. Isso impede medir a pipeline completa em
1080p no hardware disponível.

A solução **não** é baixar a meta. É separar as duas perguntas:

| Pergunta | Depende do monitor? | Como medir aqui |
|---|---|---|
| A captura acompanha a tela? | sim | resolução nativa — **já validado**: 60 fps, 0 frames perdidos |
| O encoder aguenta 1080p30? | **não** | alimentar o encoder com textura sintética de 1920×1080 |
| A latência fecha em 80 ms? | parcialmente | medir na nativa; extrapolar com o custo medido do encoder |

Codificar não se importa com o que está no monitor. Um teste de encoder com
frame sintético de 1080p é uma medição **legítima** de 1080p — e isola melhor
a variável do que capturar uma tela real.

Se for necessário validar ponta a ponta em 1080p nesta máquina, a saída é um
**driver de display virtual** (IddCx). Vale o esforço porque não é só bancada
de teste: suporte a *host headless* — máquina sem monitor físico — é recurso
de produto, e display virtual é como a concorrência resolve isso.

## Critério de saída (mensurável)

| Métrica | Alvo | Resultado |
|---|---|---|
| Captura | ≥ 30 fps nativo, sem perdas | ✅ 60 fps, 0 perdidos |
| Encode | 1080p30, < 33 ms/frame | ✅ 59,5 fps, 15,9 ms p50 (Quick Sync) |
| Transporte isolado | entrega íntegra, RTT baixo | ✅ 0,061 ms p50 em 127.0.0.1 |
| RTT do input sob carga de vídeo | responsivo | ✅ 10–15 ms p50 |
| Latência captura→exibição | estável, sem deriva | ✅ 16,8 ms p50 — **mede só parte do caminho** |
| Latência tela→exibição | — | ✅ **53,6 ms p50** a 60 fps |
| Latência glass-to-glass | < 80 ms na LAN | ✅ estimada em **64 a 79 ms** |
| Etapa 5, duas máquinas | funcionar em rede real | ✅ ver abaixo |
| Banda, uso leve (IDE aberta) | — | 0,69 Mbps |
| Banda, tela parada | < 100 kbps | ⬜ **não mensurável aqui** — ver abaixo |
| Banda, vídeo em tela cheia | < 8 Mbps | ✅ 1,90 Mbps |
| CPU no host | < 15% de um core | ✅ 8,6% de um núcleo, 63 MB de RAM |

> O RTT de 0,061 ms e o de 10–15 ms não se contradizem: o primeiro é o
> transporte sozinho, o segundo é o sistema real, onde o laço do viewer está
> ocupado decodificando quando o Ping chega. **O segundo é o número de
> produto** — é o que o usuário sente ao clicar.

### Por que a banda de tela parada não foi medida

O critério pressupõe uma tela imóvel, e esta máquina não tem uma: com o VS Code
aberto, o contador `sem-mudanca` do host ficou em **zero** ao longo de 25 s —
ou seja, a captura nunca reportou um único frame sem alteração. Bate com o
achado da etapa 1 (zero timeouts com o desktop "ocioso").

A supressão por área alterada está implementada e correta — usa dirty rects
**e** move rects, para não confundir arraste de janela com ausência de mudança.
Ela simplesmente não dispara neste workload. Medir o critério exige um desktop
controlado, sem aplicação animada.

O que se pode afirmar: **0,69 Mbps com uma IDE aberta e animando**, que é o
caso realista de uma sessão de suporte.

### Varredura de GOP (Quick Sync, 1080p30, 20 s por ponto)

| GOP | parado | desktop | vídeo |
|---|---|---|---|
| 3 s | 153 kbps | 186 kbps | 1,94 Mbps |
| 10 s | 157 kbps | 125 kbps | 1,92 Mbps |
| **30 s** | 221 kbps | **93 kbps** | **1,90 Mbps** |
| 120 s | 221 kbps | 93 kbps | 1,90 Mbps |

Não é monotônico, e a cena **imóvel custa mais que a cena em movimento**
(221 contra 93 kbps). Numa tela parada o Quick Sync gasta banda refinando
qualidade sozinho, já que sobra taxa. A conclusão que importa: **ajustar o
encoder tem retorno limitado; o ganho real vem de não chamá-lo.** Daí a
supressão por área alterada.

Adotado GOP de 30 s, escolhido por medição e não por intuição. Sobreponível com
`PCSHARE_GOP` para nova varredura em outro hardware — este resultado é de uma
Intel HD 4600 com driver antigo e não deve ser generalizado.

### Etapa 5 — resultado em rede real (30/07/2026)

Duas máquinas na mesma LAN: host em `192.168.15.3` (Windows 11, i7-4790, HD
4600), viewer em `192.168.15.6` (Windows 10, notebook). Duas sessões, uma de
22 minutos e outra de 7.

| Métrica | Loopback | **Rede real** |
|---|---|---|
| RTT do canal de controle, p50 | 5,6–6,9 ms | **8,4–12,3 ms** |
| RTT, p99 | 6,5–8,9 ms | **16,2–22,8 ms** |
| **Descartes por contrapressão** | 0 | **0** |
| Fila de saída, pico | 0 KB | 23 KB |
| Banda | 0,5–0,9 Mbps | 0,26–0,37 Mbps |
| CPU do host | 7,3% | **2,4–3,3%** de um núcleo |
| RAM do host | — | 60 MB |
| Quadros na sessão longa | — | 16.267 |

A rede acrescentou ~4 ms ao RTT, o esperado para uma ida e volta em LAN.

**O `desc 0` é o resultado que mais importa.** Em loopback ele era zero por
construção — não há perda nem limite de banda. Sobre rede real, atravessando um
roteador, continuou zero em 29 minutos somados. A contrapressão implementada na
etapa 4 nunca precisou agir.

Também confirmado: **`entrada 236` no host e `236 eventos` no viewer**, número
idêntico. O canal de volta funciona ponta a ponta, cifrado, entre identidades
distintas — host `54:34:16:…`, viewer com identidade própria.

### Pendências que a etapa 5 revelou

1. ~~**O host aceita uma única sessão.**~~ ✅ **corrigido em 31/07/2026.**
   O host agora atende sessões indefinidamente. Duas causas separadas:

   - `QuicListener::accept` consumia o listener e levava o runtime junto.
     Passou a receber `&self`, com o runtime compartilhado via `Arc` — e
     multithread, porque um runtime de thread única só avança tarefas dentro de
     `block_on` e a sessão em curso congelaria a cada `accept`.
   - Mesmo assim o host travava ao fim da sessão: as tarefas de leitura
     terminavam quando a conexão caía, mas a tarefa principal segurava a
     original do remetente do canal. O canal nunca fechava, `poll` devolvia
     "nada novo" para sempre e o host seguia capturando e codificando para
     ninguém.

   Verificado com os binários reais: quatro sessões seguidas, com uma senha
   errada no meio, e o host voltando a escutar em todas. Travado pelo teste
   `listener_aceita_varias_sessoes_em_sequencia`.
2. **Impressões digitais divergentes entre as duas sessões**
   (`B4:84:8F:…` e `8D:62:14:…`). O teste
   `identidade_recarregada_do_disco_e_a_mesma` passa e o arquivo de identidade
   foi confirmado como persistido e recarregado, o que descarta defeito na
   derivação. A hipótese em aberto é que as sessões vieram de **máquinas
   diferentes** — provavelmente uma terceira máquina em `192.168.0.249`, atrás
   de outro roteador, cujo tráfego de saída passa por NAT mesmo com o ping
   falhando. Isso também explicaria o timeout inicial do notebook: a vaga única
   de sessão já estava ocupada. **Não confirmado.**

### A latência real é muito maior que a medida internamente

Os 16,8 ms sempre mediram **captura → quadro decodificado**. O cronômetro
começava quando a captura entregava o quadro — e é aí que mora o erro: o pixel
já tinha mudado na tela bem antes disso.

O modo `latencia` fecha o laço pela própria tela. Um marcador troca de cor e
esperamos essa troca reaparecer num quadro decodificado, percorrendo
**descarga na tela → compositor do Windows → captura DXGI → encode → rede →
decode**:

Decompondo com os dois cronômetros no mesmo evento, e comparando com a captura
medida isoladamente (modo `latencia-captura`, sem encoder nem rede):

| | tela → captura | captura → decodificação | total |
|---|---|---|---|
| 30 fps | 76,2 ms | 7,4 ms | 83,5 ms |
| 60 fps, antes | 60,0 ms | 6,7 ms | 66,7 ms |
| **60 fps, depois** | **43,4 ms** | 10,2 ms | **53,6 ms** |
| *captura isolada* | *33,6 ms* | — | — |

Duas conclusões, e nenhuma era o palpite inicial.

**A pipeline não é o gargalo.** Encode, rede e decode somam 7 a 10 ms. O resto
é o caminho até o quadro chegar às nossas mãos. Continuar otimizando codec ou
transporte rende quase nada.

**Mas 26 ms daquele trecho eram nossos.** A captura isolada custa 33,6 ms; no
pipeline o mesmo trecho marcava 60 ms. A diferença era um **batimento no
controle de ritmo**: a comparação exata `elapsed < period` descartava o quadro
que chegasse meio milissegundo adiantado, e o próximo só viria um intervalo
inteiro depois. Um jitter de 0,5 ms custava 16,7 ms. Uma tolerância de
`period/8` recuperou 17 ms.

Restam ~10 ms nossos, provavelmente por segurar o quadro do DXGI durante o
encode. O piso do sistema — DWM mais Desktop Duplication — é **33,6 ms** e não
depende de nós.

Com a apresentação no viewer e a resposta física dos monitores (10 a 25 ms,
fora do alcance de software), o glass-to-glass estimado é de **64 a 79 ms**,
dentro da meta.

### Orçamento por frame, medido a 1360×768

O desvio pela CPU foi **eliminado na Fase 1**. Ver decisão D3.

| | leitura GPU | conversão | encode | total |
|---|---|---|---|---|
| memória de sistema | 5,37 ms | 2,06 ms | 4,29 ms | **11,72 ms** |
| **GPU direto** | — | 0,08 ms | 3,57 ms | **3,65 ms** |

3,2× mais rápido, 8 ms por quadro. Comparação reproduzível com `--cpu-path`.

> **Achado da etapa 4 que altera o design:** cada estágio passou isolado, e a
> integração ainda assim falhou. Sem pacing nem descarte, a latência crescia
> sem limite — 283 ms, 1021 ms, 1473 ms, 2075 ms, 2910 ms em doze segundos. O
> host codificava a 58 fps enquanto o viewer exibia 49, e a diferença virava
> fila. Numa tela remota, **descartar é melhor que atrasar**: o host respeita o
> alvo de fps e para quando a fila de saída passa de 512 KB; o viewer
> decodifica tudo (H.264 é interdependente) mas exibe só o quadro mais recente.
> Com isso a latência ficou estável em 16,5 ms.

> **Achado da etapa 3 que altera o design:** um laço de espera com
> `sleep(200 µs)` custou 1,27 ms de RTT em 127.0.0.1; com `yield_now`, 0,061 ms.
> O Windows não honra sleeps de sub-milissegundo. Ver decisão D8 — a saída não é
> spin, é espera bloqueante em evento.

> **Achado da etapa 1 que altera o design:** com o desktop ocioso, a captura
> registrou **zero timeouts** — algo redesenha 60 vezes por segundo, ainda que
> numa área ínfima (0,3% da tela). A suposição "tela parada, banda zero" não se
> confirma. A supressão de envio precisa ser decidida por **área alterada**, e
> não pela ausência de frame novo.

### Como medir latência glass-to-glass corretamente

Não confie em timestamps internos — eles não medem o que o usuário sente.

1. Abra um cronômetro com milissegundos na tela do **host**.
2. Coloque a janela do **viewer** ao lado, exibindo esse mesmo cronômetro.
3. Fotografe as duas telas juntas com um celular.
4. A diferença entre os dois números **é** a latência glass-to-glass.
5. Repita 10 vezes, use a mediana.

Isso captura tudo: captura, encode, rede, decode, e o tempo de apresentação do
compositor — inclusive os custos que instrumentação interna não enxerga.

---

## Decisão ao final do sprint

- **Bateu as metas** → o resto do projeto é trabalho conhecido. Segue para a Fase 1.
- **Chegou perto** (ex.: 25 fps, 110 ms) → identificar o gargalo dominante e
  decidir se é ajuste ou problema estrutural.
- **Ficou longe** → parar e reavaliar todo o plano de negócio **antes** de
  gastar meses. Este é o propósito do spike.

---

## Ordem de ataque sugerida

Cada etapa é verificável isoladamente. Não avance com uma etapa "quase funcionando".

1. **Captura pura.** DXGI Desktop Duplication num loop, descartando os frames.
   Medir: quantos frames/s a API entrega e quantas dirty rects por frame.
   *Já revela se a HD 4600 aguenta o caminho.*
2. **Encode.** Ligar o Media Foundation H.264. Gravar 10 s em arquivo e abrir
   no player. Medir: tempo de encode por frame.
3. **Transporte local.** Host e viewer no mesmo PC, via `127.0.0.1`.
   Elimina a rede como variável.
4. **Decode e exibição.** Primeira imagem na tela. Medir latência aqui.
5. **Duas máquinas na LAN.** Medir de novo — a diferença é o custo da rede.
6. **Input de volta.** Mouse e teclado do viewer para o host.

---

## Notas de implementação

- Manter host e viewer como **um único processo cada**. A separação
  serviço/agent (decisão D5) vem na Fase 3; antecipá-la aqui só adiciona atrito.
- Todo I/O de rede passa pelo trait `Transport`. Nenhum `TcpStream` deve
  aparecer no código de captura ou de encode. Ver decisão D4.
- Instrumentar desde o primeiro dia: fps, bytes enviados, tempo de encode.
  Otimizar sem medir é perda de tempo garantida.
