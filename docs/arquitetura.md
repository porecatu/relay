# Arquitetura

Documento técnico central do relay. Os ADRs justificam *por que*; este documento descreve *como* as peças se encaixam. Ainda **não há código**: o que está aqui é o desenho que a etapa R1 do [roadmap](roadmap.md) implementa, e onde o código divergir, a seção ganha um bloco **Na implementação**, como no Porecatu.

O contrato externo — caminhos, envelope, códigos, limites mínimos — está na [especificação do protocolo, §1 a §7](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md), e não é repetido aqui.

---

## 1. Visão geral

```
                            +-------------------------------------------------+
                            |                 porecatu-relay                  |
                            |                                                 |
  host (desktop)            |   +-------------+       Registry                |
  wss /v0/host  ----------->|-->| host task   |<----- host_id -> HostHandle   |
                            |   |  reader     |       (Mutex<HashMap>)        |
                            |   |  writer     |              ^                |
                            |   |  channels   |              | lookup         |
                            |   +------+------+              |                |
                            |          | ToHost (mpsc,       |                |
                            |          |  limitada)          |                |
                            |   +------+------+       +------+------+         |
  cliente (celular)         |   | client task |  ...  | client task |         |
  wss /v0/client/<id> ----->|-->|  reader     |       |  reader     |<--------|-- outro cliente
                            |   |  writer     |       |  writer     |         |
                            |   +-------------+       +-------------+         |
                            |          ^ ToClient (mpsc, limitada)            |
                            +-------------------------------------------------+
```

- **Uma task por conexão WebSocket**, de host ou de cliente, no runtime multi-thread do `tokio` ([ADR-0001](adr/0001-rust-tokio-e-axum.md)). Cada uma divide o socket em metade de leitura e metade de escrita.
- **O registro** é o único estado compartilhado: um `HashMap<HostId, HostHandle>` sob um `Mutex` síncrono, **nunca segurado através de um `.await`**.
- **As tasks conversam por canais `mpsc` limitados.** Nenhuma fila do relay é ilimitada.
- **Nada em disco** ([ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md)).

## 2. Subida

1. Lê e valida o `relay.toml` ([PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.1 a RF-1.3). Qualquer erro, inclusive limite abaixo do piso da especificação, impede a subida.
2. Carrega os tokens e guarda **só o resumo BLAKE2s-256** de cada valor, mais o nome ([ADR-0003](adr/0003-registro-do-host-por-chave-e-token-do-operador.md)). O valor em claro sai da memória depois disso.
3. Aplica a regra do TLS ([ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md)): com `[tls]`, monta a configuração `rustls`; sem ela, confere loopback ou `behind_proxy`.
4. Monta o roteador `axum`: `/v0/host`, `/v0/client/{host_id}`, `/v0/presence/{host_id}` (leitura do mapa de hosts, sem canal), `/healthz`; `/metrics` num listener separado, se ligado.
5. Instala o tratador de sinais, que dispara o `CancellationToken` do encerramento (§7).

## 3. Conexão de host

```
GET /v0/host  ── Authorization? ──no──> 401
                     │ sim
                  upgrade
                     │
           challenge(nonce) ──> host
                     │
   register(key, sig) <── host      (timeout: register_timeout_seconds)
                     │
       verify_strict? ──no──> close 4001
                     │ sim
   host_id = porecatu_protocol::host_id(key)
                     │
   Registry.insert(host_id, handle{generation})  ── havia outro? ──> antigo: Replaced
                     │
         registered(host_id) ──> host
                     │
               laço da host task
```

A **host task** é dona de três coisas:

- a metade de escrita do WebSocket do host;
- a **tabela de canais**: `channel → Sender<ToClient>` do cliente correspondente, mais o contador do próximo `channel` (monotônico, nunca reaproveitado — [especificação §4](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md));
- o receptor `Receiver<ToHost>`, por onde chegam pedidos das client tasks: abrir canal, dados de um canal, canal fechado.

Ela faz `select!` sobre: quadro do WebSocket do host, mensagem em `ToHost`, tique do ping (§6) e cancelamento.

O `HostHandle` guardado no registro tem o `Sender<ToHost>`, uma **geração** (contador global) e o nome do token. Quando a host task termina, ela se tira do registro **só se a geração ainda for a dela** — senão apagaria o registro do host que a substituiu.

## 4. Conexão de cliente

1. `GET /v0/client/{host_id}`: valida o formato do `host_id` (32 caracteres base32 minúsculos). Inválido: 400 antes do upgrade.
2. Upgrade. Procura o `host_id` no registro. Ausente: manda `host-unavailable` e fecha com `1000`.
3. Pede à host task, por `ToHost::Open { to_client, reply }`, que abra um canal. A host task confere o limite de canais, aloca o `channel`, grava `to_client` na tabela, manda `channel-opened` ao host e responde pelo `oneshot`. Limite estourado: responde recusa, e o cliente recebe `closed { limit-exceeded }`.
4. Laço da **client task**, `select!` sobre: quadro do WebSocket do cliente (`data` → `ToHost::Data { channel, payload }`), mensagem em `ToClient` (dados do host → `data`; fechamento → `closed { reason }`), tique do ping e cancelamento.
5. O cliente fecha o WebSocket: a client task manda `ToHost::Closed { channel }`, e a host task tira o canal da tabela e manda `channel-closed { normal }` ao host.

## 5. Roteamento e contrapressão

O relay nunca descarta um `payload` em silêncio: um registro Noise perdido quebra a sessão ([especificação §6](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). A política é diferente em cada sentido, porque o custo de esperar é diferente.

| Sentido | Fila | Fila cheia | Por quê |
|---|---|---|---|
| **Cliente → host** | `ToHost`, uma por host, capacidade `host_queue_frames` | A client task **espera** (`send().await`). A contrapressão sobe até o TCP do cliente | Esperar só atrasa aquele cliente; o host lento é o gargalo de todos, e não há a quem culpar |
| **Host → cliente** | `ToClient`, uma por canal, capacidade `channel_queue_frames` | A host task **não espera** (`try_send`): fecha **aquele canal** com `limit-exceeded` | Se a host task esperasse um celular lento, todos os canais do host parariam junto |

Além das filas:

- **Banda por canal**: balde de fichas por canal, nos dois sentidos, com taxa `channel_bandwidth_kbps` e rajada `channel_burst_kib`. Estouro sustentado fecha o canal com `limit-exceeded`. O piso da especificação (2 Mbit/s) é o mínimo aceito na configuração.
- **Tamanho de quadro**: o limite do WebSocket do `axum` é configurado em `max_frame_bytes` (piso 70 000); quadro maior fecha com `1009`.
- **Ordem**: cada canal tem uma fila por sentido e é tratado por uma task só em cada ponta, então os quadros saem na ordem em que entraram.

O `payload` atravessa o relay como `Bytes` dentro do tipo opaco do [ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md): sem cópia além da do decodificador CBOR, sem inspeção.

## 6. Keepalive e prazos

| Prazo | Valor | Onde |
|---|---|---|
| `Ping` do relay | a cada 25 s | Toda task de conexão (especificação §1) |
| Sem `Pong` | fecha em 60 s | Toda task de conexão |
| Registro do host | `register_timeout_seconds`, default 10 | Host task, antes do registro |
| Encerramento | `shutdown_grace_seconds`, default 10 | §7 |

Os prazos são `tokio::time::sleep_until` dentro do `select!` de cada task; não há thread de timer.

## 7. Encerramento

1. O sinal dispara o `CancellationToken` raiz; o listener para de aceitar.
2. Cada client task manda `closed { normal }` ao cliente e fecha com `1001`.
3. Cada host task fecha com `1001` (os `channel-closed` seriam inúteis: a conexão do host fecha junto).
4. Passado `shutdown_grace_seconds`, o processo sai mesmo com tasks pendentes.

## 8. Substituição de host

Quando um `register` válido chega para um `host_id` já registrado ([ADR-0003](adr/0003-registro-do-host-por-chave-e-token-do-operador.md) §3):

1. Sob o `Mutex` do registro, o `HostHandle` novo entra no lugar do antigo, e o antigo é retirado — uma operação, sem instante sem dono.
2. Fora do `Mutex`, o relay manda `ToHost::Replaced` ao antigo.
3. A host task antiga manda `ToClient::Close(host-replaced)` a cada canal da tabela dela e fecha o WebSocket do host com `4002`.
4. Cliente que chega depois do passo 1 já vai para o host novo.

## 9. Logs e métricas

- `tracing` com campos fechados: `host_id`, `channel`, `token_name`, `reason`, `bytes_in`, `bytes_out`, `duration_ms`, `peer` (pela política de `[log] ip_addresses`). Nenhum outro campo é aceito no código de conexão; o teste do RF-1.18 confere.
- Métricas (`[metrics] enabled`): contadores e histogramas agregados, sem rótulo de identidade.

## 10. Estratégia de teste

- **Unidade**: decodificação de envelope contra os vetores de `porecatu-protocol`; cálculo do `host_id`; regra de subida do TLS; validação da configuração (pisos, chaves desconhecidas).
- **Integração**, em processo, com host e cliente falsos em `tokio-tungstenite`: registro (ok, assinatura errada, timeout, token errado), substituição, ciclo do canal, ordem de mil quadros, cada limite estourado com o código certo, encerramento.
- **Log cego**: sessão com marcador conhecido no `payload`, filtro em `trace`, saída capturada; o marcador não pode aparecer.
- **Carga** (etapa R3): mil hosts ociosos e cem canais ativos, medindo memória por canal e latência acrescentada contra as métricas do [PRD-000](prd/prd-000-visao-de-produto.md).
