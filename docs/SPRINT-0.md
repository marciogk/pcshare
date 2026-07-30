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
| Transporte | entrega íntegra, RTT baixo | ✅ 0,061 ms p50 em 127.0.0.1 |
| Latência captura→exibição | estável, sem deriva | ✅ 16,5 ms p50 / 21 ms p99, 1360×768 |
| Latência glass-to-glass | < 80 ms na LAN | ⬜ **falta** — exige o protocolo com câmera |
| Banda, tela parada | < 100 kbps | ⚠️ 123 kbps; keyframe periódico domina |
| Banda, vídeo em tela cheia | < 8 Mbps | ✅ 1,95 Mbps |
| CPU no host | < 15% de um core | ⬜ não medido |

### Orçamento por frame, medido a 1360×768

```
leitura da GPU   4,1 ms   ← maior item; é o desvio que a decisão D3 elimina
BGRA → NV12      1,9 ms   ← idem
encode           4,5 ms
```

Os dois primeiros somam 6 ms que a pipeline definitiva não deveria gastar: a
textura sai da GPU e volta para ela logo em seguida. A 1080p esse desperdício
mais que dobra.

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
