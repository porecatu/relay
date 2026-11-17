# Arquitetura

Documento técnico central do relay. Os ADRs justificam *por que*; este documento descreve *como* as peças se encaixam no código de hoje (`src/`). Ele nasceu como o desenho da etapa R1 do [roadmap](roadmap.md), antes do código; onde a implementação saiu diferente do desenho, a seção diz o que o código faz e ganha um bloco **Na implementação** com a diferença, como no Porecatu.

O contrato externo — caminhos, envelope, códigos, limites mínimos — está na [especificação do protocolo, §1 a §7](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md), e não é repetido aqui.

---

## 0. Mapa dos módulos

Um pacote, `porecatu-relay`, com uma biblioteca (`src/lib.rs`, usada pelos testes de integração) e o binário (`src/main.rs`).

| Módulo | O que faz |
|---|---|
| `main.rs` | Linha de comando, carga da configuração e do par TLS, logs, runtime; códigos de saída `0` (ok), `1` (falha em execução: porta ocupada, servidor), `2` (uso ou configuração inválida: o relay não subiu) |
| `cli` | Parsing à mão de `--config`, `--version`, `--help` e `token new [--name]` |
| `config` | `relay.toml` + variáveis `PORECATU_RELAY_*` → `Settings`; validação contra os pisos (`config::validate`); `config::default_path` |
| `token` | `TokenSet`: resumos BLAKE2s-256, comparação em tempo constante, `parse_bearer`, `generate_token` |
| `server` | `ServerState` (o estado compartilhado), o roteador `axum` das quatro rotas, `serve`/`serve_tls`, `send_bounded`, `close_with` |
| `host` | A task da conexão de host: desafio, registro, tabela de canais, keepalive |
| `client` | A task da conexão de cliente: um canal num host, contrapressão, banda do sentido cliente → host |
| `registry` | `Registry<HostHandle>` (o mapa de hosts) e `ChannelAllocator` (números de canal de uma conexão de host) |
| `wire` | Adaptador do envelope do `porecatu-protocol`: decodifica por sentido, codifica, dá o código de fechamento de cada erro |
| `payload` | `OpaquePayload`, o tipo opaco do `payload` ([ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md)) |
| `peer` | IP do par (socket ou `X-Forwarded-For` de proxy confiável), IP para log, chave dos limites por IP |
| `limits` | `TokenBucket` (banda por canal e sentido) e `RateLimiter` (limites por IP, GCRA) |
| `tls` | `rustls::ServerConfig` a partir dos arquivos PEM e a recarga por sondagem |
| `logging` | `tracing-subscriber` em `stderr`, texto ou JSON, com os alvos das dependências de rede travados em `info` |
| `metrics` | Contadores agregados e o listener separado de `GET /metrics` |
| `shutdown` | Sinais do sistema e o prazo de `shutdown_grace_seconds` |

O envelope CBOR, o cálculo do `host_id`, os códigos de fechamento e os prazos de keepalive vêm do crate [`porecatu-protocol`](https://github.com/porecatu/protocol) (`porecatu_protocol::envelope`, `porecatu_protocol::host_id`); o relay não os reimplementa.

> **Na implementação (2026-11-16):** o `porecatu-protocol` entra como dependência git fixada num commit do `main` do protocolo (`rev = "0909e7c…"`), até o crate ser publicado com versão; aí vira versão exata ([ADR-0001](adr/0001-rust-tokio-e-axum.md)).

## 1. Visão geral

```
                            +-------------------------------------------------+
                            |                 porecatu-relay                  |
                            |                                                 |
  host (desktop)            |   +-------------+    Registry<HostHandle>       |
  wss /v0/host  ----------->|-->| host task   |<-- host_id -> (Generation,    |
                            |   |  socket     |               HostHandle)     |
                            |   |  Channels   |    (std::sync::Mutex)         |
                            |   |  (tabela)   |              ^                |
                            |   +------+------+              | lookup         |
                            |          ^ HostCommand (mpsc,  |                |
                            |          |  limitada, 1/host)  |                |
                            |   +------+------+       +------+------+         |
  cliente (celular)         |   | client task |  ...  | client task |         |
  wss /v0/client/<id> ----->|-->|  socket     |       |  socket     |<--------|-- outro cliente
                            |   +-------------+       +-------------+         |
                            |          ^ OpaquePayload (mpsc, limitada,       |
                            |            1/canal) + oneshot<CloseReason>      |
                            +-------------------------------------------------+
```

- **Uma task por conexão WebSocket**, de host ou de cliente, no runtime multi-thread do `tokio` ([ADR-0001](adr/0001-rust-tokio-e-axum.md)). A task é dona do socket inteiro e faz leitura e escrita no mesmo laço `select!`.
- **O estado compartilhado** mora em `server::ServerState`: o registro de hosts (`Registry<HostHandle>`), os três limitadores por IP, as métricas, o contador de conexões vivas e o `CancellationToken` do encerramento. Todo `Mutex` é síncrono (`std::sync::Mutex`) e **nunca é segurado através de um `.await`**: os métodos do registro e dos limitadores são síncronos.
- **As tasks conversam por canais `mpsc` limitados.** Nenhuma fila do relay é ilimitada.
- **Nada em disco** ([ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md)).

> **Na implementação:** o desenho dizia que o registro era o único estado compartilhado e que cada task dividia o socket em duas metades. No código, `ServerState` também guarda limitadores, métricas e o contador de conexões (todos sem dado de conteúdo), e o socket não é dividido: um `select!` só lê e escreve.

## 2. Subida

`main.rs`, em ordem:

1. Lê a linha de comando. Sem `--config`, usa `config::default_path()`: `/etc/porecatu-relay/relay.toml` no Linux; nas outras plataformas, `relay.toml` **ao lado do executável** (o diretório corrente só se o caminho do executável não puder ser determinado) — ver a nota no [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.1.
2. `config::load` lê o arquivo, aplica as variáveis `PORECATU_RELAY_*` e valida tudo: chave desconhecida, valor abaixo do piso, nenhum token, regra do TLS ([ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md)). Os tokens já saem dali como `TokenDigest` (nome + resumo BLAKE2s-256); o valor em claro não chega ao estado do processo. Qualquer erro: mensagem com arquivo, linha (ou variável) e chave, e saída `2`.
3. Com `[tls]`, lê o par PEM e monta o `rustls::ServerConfig` (`tls::load_from_files`) **antes** de escutar: par ilegível é erro de configuração (saída `2`), não um relay que escuta e falha todo handshake.
4. Liga os logs (`logging::init`) e o runtime multi-thread.
5. Abre o listener de `[server] bind` e, com `[metrics] enabled`, o de `[metrics] bind` (porta ocupada: saída `1`, com a chave na mensagem).
6. Instala o tratador de sinais (`shutdown::cancel_on_signal`), que cancela o `CancellationToken` raiz (§7).
7. Serve o roteador `axum` (`server::router`): `/v0/host`, `/v0/client/{host_id}`, `/v0/presence/{host_id}` (leitura do registro, sem canal), `/healthz`. Em texto simples (`server::serve`) ou com TLS (`server::serve_tls`, via `axum-server`, com a tarefa de recarga do certificado). `/metrics` fica no listener separado (`metrics::serve`).

### Antes do upgrade

Cada rota resolve o IP do par uma vez (`peer::resolve_peer`, §9) e o usa para os limites e para os logs. A ordem das checagens é contrato:

| Rota | Checagens, em ordem |
|---|---|
| `/v0/host` | `429` (`host_attempts_per_ip_per_minute`), `401` (token do operador, com `WWW-Authenticate: Bearer`), upgrade |
| `/v0/client/{host_id}` | `400` (`host_id` mal formado, `HostId::parse`), `429` (`client_connects_per_ip_per_minute`), upgrade |
| `/v0/presence/{host_id}` | `400`, `429` (`presence_per_minute_per_ip`), `204` ou `404` sem corpo |
| `/healthz` | nenhuma: `200 ok` |

O limite por IP do host vem **antes** do token: quem tenta adivinhar tokens é contado a cada tentativa, certa ou errada.

## 3. Conexão de host

```
GET /v0/host ── 429? ── Authorization? ──no──> 401
                              │ sim
                           upgrade
                              │
            challenge(nonce de 32 bytes, getrandom) ──> host
                              │
            register(key, sig) <── host      (prazo: register_timeout_seconds)
                              │     atraso: 4005 · outro envelope/CBOR inválido: 4004
                              │
      wire::verify_register (verify_strict) ──falha──> close 4001
                              │ ok
            host_id = porecatu_protocol::host_id::host_id(key)
                              │
            Registry::register ── Full ──> close 4003 (max_hosts)
                              │  └─ Replaced { old } ──> old.notify_replaced() (fora do lock)
                              │
            registered(host_id) ──> host
                              │
                      laço da host task
```

A **host task** (`host::run`) é dona de:

- o socket do host;
- a **tabela de canais** (`Channels`): o `ChannelAllocator` (próximo número, monotônico, nunca reaproveitado — [especificação §4](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)) e, por canal vivo, um `ChannelEntry` com o `Sender<OpaquePayload>` da fila do cliente, o `oneshot::Sender<CloseReason>` do motivo de fechamento e o `TokenBucket` do sentido host → cliente;
- o `Receiver<HostCommand>`, por onde chegam os pedidos das client tasks: `OpenChannel { to_client, close, reply }`, `ClientData { channel, payload }` e `ClientGone { channel, reason }`. Uma fila só leva os três, então o que um cliente mandou chega na ordem em que mandou, e um `ClientGone` nunca ultrapassa os dados de antes dele.

Ela faz `select!` sobre: cancelamento (encerramento), sinal de substituição, prazo do `Pong`, tique do `Ping`, a fila de comandos e o socket do host.

O `HostHandle` guardado no registro tem só duas coisas: o `Sender<HostCommand>` e o `CancellationToken` de "substituído". A **geração** (`Generation`, contador do registro) fica ao lado do handle, na entrada do mapa (`HashMap<HostId, (Generation, HostHandle)>`). Quando a host task termina, ela se tira do registro com `Registry::unregister(host_id, generation)`, que **só remove se a geração ainda for a dela** — senão apagaria o registro do host que a substituiu. Isso acontece antes do handshake de fechamento, para uma consulta de presença não ver um host que já está saindo.

Do host registrado, o relay aceita só `channel-data` e `close-channel`. `channel-data` ou `close-channel` para um canal já fechado é ignorado; para um número nunca dado (`ChannelAllocator::was_issued`), ou um segundo `register`, fecha com `4004`. Envelope que não decodifica: `4004`. Quadro de texto: `1003`.

> **Na implementação:** o desenho punha o nome do token e a geração dentro do `HostHandle`. No código, o nome do token fica na host task (só para os logs dela) e a geração na entrada do registro; e o registro cheio (`[limits] max_hosts`) fecha um host novo com `4003`, como manda a especificação (substituição não conta).

## 4. Conexão de cliente

1. `GET /v0/client/{host_id}`: `HostId::parse` valida o formato; inválido é `400` antes do upgrade. Depois, o limite por IP (`429`).
2. Upgrade. `Registry::lookup` procura o `host_id`. Ausente: manda `host-unavailable` e fecha com `1000`.
3. Pede à host task, por `HostCommand::OpenChannel { to_client, close, reply }` (esperando vaga na fila com `reserve`), que abra um canal. A host task confere o limite de canais, aloca o número, guarda a entrada na tabela, manda `channel-opened` ao host e responde pelo `oneshot`. Limite estourado: o cliente recebe `closed { limit-exceeded }` e `4003`; o host não ouve nada. Se a host task está saindo (substituída ou caída entre a busca e a resposta), o cliente procura o host de novo, até 3 vezes (`OPEN_ATTEMPTS`); na última, `host-unavailable`.
4. Laço da **client task** (`client::run`), `select!` sobre: cancelamento, o `oneshot` do motivo de fechamento vindo do host, prazo do `Pong`, tique do `Ping`, a fila de dados do canal (`data` ao cliente), a vaga na fila do host (quando há um `data` esperando) e o socket do cliente (só quando nada espera).
5. O cliente fecha o WebSocket (ou para de responder ao `Ping`): a client task manda `HostCommand::ClientGone { channel, reason: normal }`, e a host task tira o canal da tabela e manda `channel-closed { normal }` ao host. Envelope inválido do cliente: `closed { protocol-error }` + `4004` ao cliente e `channel-closed { protocol-error }` ao host; quadro de texto: `1003` sem `closed` antes, e o host ouve `protocol-error`.
6. O host fecha o canal (`close-channel`): o motivo chega à client task pelo `oneshot`. Se o motivo é `normal` ou `host-rejected`, o que o host mandou antes ainda vai ao cliente, em ordem, antes do `closed`.

## 5. Roteamento e contrapressão

O relay nunca descarta um `payload` em silêncio: um registro Noise perdido quebra a sessão ([especificação §6](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). A política é diferente em cada sentido, porque o custo de esperar é diferente.

| Sentido | Fila | Fila cheia | Por quê |
|---|---|---|---|
| **Cliente → host** | A fila de comandos da host task (`mpsc::Sender<HostCommand>`), **uma por host**, compartilhada pelos clientes dele, capacidade `[limits] host_queue_frames` | **Contrapressão, não fechamento.** A client task guarda o `data` e espera vaga (`Sender::reserve` dentro do `select!`). Enquanto espera, não lê mais nada do cliente, e a pressão sobe até o TCP do celular. Continua entregando o que o host manda, respondendo ao keepalive e obedecendo ao encerramento. O prazo do `Pong` do cliente fica suspenso durante a espera e recomeça quando a leitura volta | Esperar só atrasa aquele cliente; o host lento é o gargalo de todos, e não há a quem culpar. A host task sempre esvazia a fila e o socket no mesmo laço, então quem espera vaga espera uma task que anda |
| **Host → cliente** | Uma fila de `OpaquePayload` **por canal**, capacidade `[limits] channel_queue_frames` | A host task **não espera** (`try_send`): fecha **aquele canal** com `limit-exceeded`. O motivo vai por um `oneshot` à parte, porque a fila em que ele teria de ir é a que está cheia | Se a host task esperasse um celular lento, todos os canais do host parariam junto |

A fila do host cheia **não** é limite estourado: nada fecha e nada é descartado; o teste `a_full_host_queue_makes_the_client_wait_and_closes_nothing` (`tests/limits.rs`) fixa isso. Só a fila de um canal (host → cliente) fecha com `limit-exceeded`.

Além das filas:

- **Banda por canal**: um `TokenBucket` por canal **e por sentido**, contado em bytes de `payload`, com taxa `channel_bandwidth_kbps` (kbit/s decimais, `× 1000 / 8` bytes/s) e capacidade `channel_burst_kib`. O balde começa cheio. A client task conta o sentido cliente → host; a host task, o sentido host → cliente. Um quadro que o balde recusa fecha o canal com `limit-exceeded` na hora (o relay não espera o balde encher). Por isso a rajada tem de caber num quadro inteiro: `channel_burst_kib` abaixo de 69 KiB (70 000 bytes) impede a subida.
- **Tamanho de quadro**: o limite do WebSocket do `axum` (`max_frame_size` e `max_message_size`) é `[limits] max_frame_bytes`, que é **exatamente** 70 000 — a especificação o fixa como piso **e** teto, e outro valor impede a subida. Quadro maior fecha com `1009`, de qualquer lado. Um `data` do cliente cujo `channel-data` passaria do máximo (o envelope do host tem o número do canal a mais) fecha o canal com `limit-exceeded` (`WireError::ForwardTooLarge`).
- **Escrita com prazo**: toda escrita num socket passa por `server::send_bounded`, que desiste depois de `IO_TIMEOUT` (2 s): um par que nunca lê não segura a task. O fechamento (`server::close_with`) manda o `Close` e lê até a resposta do par, com o mesmo prazo, para o `Close` não se perder num reset.
- **Ordem**: cada canal tem uma fila por sentido e é tratado por uma task só em cada ponta, então os quadros saem na ordem em que entraram.

O `payload` atravessa o relay como `OpaquePayload`, que envolve o `Payload` do `porecatu-protocol` (uma fatia `Bytes` do quadro recebido, sem cópia): sem inspeção, sem `Debug`, `Display`, `Serialize` nem `Clone` ([ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md)).

> **Na implementação:** o desenho falava em "estouro sustentado" da banda; no código, o primeiro quadro que não cabe no balde já fecha o canal. E a rajada passou a ter piso (um quadro), que o desenho não previa.

## 6. Keepalive e prazos

| Prazo | Valor | Onde |
|---|---|---|
| `Ping` do relay | a cada 25 s (`PING_INTERVAL_MS` do `porecatu-protocol`) | Host task e client task (especificação §1) |
| Sem `Pong` | derruba em 60 s (`PONG_TIMEOUT_MS`), **sem** código de fechamento | Host task e client task. Na client task, suspenso enquanto um `data` espera vaga na fila do host (§5) |
| Registro do host | `register_timeout_seconds`, default e piso 10 | Host task, antes do registro (`4005`) |
| Escrita e handshake de fechamento | 2 s (`server::IO_TIMEOUT`) | Toda escrita em socket |
| Encerramento | `shutdown_grace_seconds`, default 10 | §7 |

Os prazos são `tokio::time::sleep_until` dentro do `select!` de cada task; não há thread de timer. Os períodos de keepalive estão em `server::Timing`, que os testes encurtam.

## 7. Encerramento

1. O sinal (`SIGINT`/`SIGTERM`; no Windows, Ctrl+C, fechamento do console e desligamento do sistema) cancela o `CancellationToken` raiz; o servidor para de aceitar e cancela o token de `ServerState`.
2. Cada client task manda `closed { normal }` ao cliente e fecha com `1001`.
3. Cada host task fecha com `1001`, inclusive a que ainda espera o `register` (os `channel-closed` seriam inúteis: a conexão do host fecha junto).
4. `server::serve` e `serve_tls` só terminam quando o contador de conexões vivas chega a zero (o `axum` e o `axum-server` não esperam as tasks de WebSocket sozinhos). `shutdown::run` limita essa espera a `shutdown_grace_seconds`: passado o prazo, o processo sai mesmo com conexões abertas, com o log `grace-elapsed`. Nos dois casos, a saída é `0`.

## 8. Substituição de host

Quando um `register` válido chega para um `host_id` já registrado ([ADR-0003](adr/0003-registro-do-host-por-chave-e-token-do-operador.md) §3):

1. Sob o `Mutex` do registro, `Registry::register` põe o `HostHandle` novo no lugar do antigo, com geração nova, e devolve o antigo (`RegisterOutcome::Replaced { old }`) — uma operação, sem instante sem dono.
2. Fora do `Mutex`, a task nova chama `old.notify_replaced()`, que cancela o token de "substituído" do handle antigo.
3. A host task antiga acorda, manda a cada canal da tabela dela `host-replaced` pelo `oneshot` (cada cliente recebe `closed { host-replaced }` e o fechamento com `1000`) e fecha o WebSocket do host com `4002`. O `unregister` dela não remove nada: a geração já não é a dela.
4. Cliente que chega depois do passo 1 já vai para o host novo.

## 9. IP do par, logs e métricas

**IP do par** (`peer::resolve_peer`), usado nos logs **e** nos limites por IP:

- Socket fora de `[server] trusted_proxies`: o IP do socket; `X-Forwarded-For` é ignorado.
- Socket em `trusted_proxies`: a entrada **mais à direita** do `X-Forwarded-For` que não está em `trusted_proxies` (cada proxy acrescenta à direita o endereço que viu; o que está à esquerda ninguém confiável atesta). Vários cabeçalhos `X-Forwarded-For` valem como um só, unidos por vírgula.
- Cabeçalho ausente, com valor que não é texto, com entrada vazia ou com entrada que não é um IP puro (com porta, por exemplo): o IP do socket.
- Cadeia em que **todas** as entradas estão em `trusted_proxies`: o IP do socket (decisão do dono de 2026-11-16, [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.13).
- IPv6 mapeado em IPv4 (`::ffff:a.b.c.d`) vira IPv4 antes de qualquer comparação.
- `trusted_proxies` vale com ou sem `behind_proxy`: as duas chaves são independentes no código.

Nos limites por IP, um IPv6 conta pela rede `/64` (`peer::limit_key`). Os limitadores (`limits::RateLimiter`) usam GCRA: limite `N` por minuto é rajada `N` e uma vaga a cada `60 s / N`; cada um lembra no máximo 65 536 chaves e esquece a mais antiga quando enche.

**Logs** (`logging`): `tracing` em `stderr`, texto ou JSON. O nível configurado vale para o alvo `porecatu_relay`; `tungstenite`, `tokio_tungstenite`, `hyper`, `hyper_util`, `h2`, `rustls`, `axum`, `axum_server`, `tower` e `tower_http` ficam em `info` no máximo, qualquer que seja o nível; `RUST_LOG` é ignorado, e não há ponte de `log` para `tracing`. Os campos dos eventos de conexão são fechados: `host_id`, `channel`, `token_name`, `reason`, `bytes_in`, `bytes_out`, `duration_ms`, `peer` (pela política de `[log] ip_addresses`, via `peer::ip_for_log`). Fora das conexões, dois eventos têm campo próprio, sem dado de ninguém: a recarga do certificado (`event = "tls_reloaded"` ou `"tls_reload_failed"`) e a subida (`tls = true|false`). O teste `tests/blind_log.rs` roda uma sessão inteira em `trace`, em texto e em JSON, e confere que o marcador do `payload` não aparece de nenhuma forma (texto, hex, base64, truncado), que o token e o cabeçalho `Authorization` também não, e que cada campo de cada linha JSON está na lista permitida.

**Métricas** (`metrics`, `[metrics] enabled`, desligadas por padrão): texto Prometheus escrito à mão, num listener separado que só responde `GET /metrics`, em HTTP simples. Rótulos só de conjuntos fechados que o próprio relay define:

| Série | Tipo | Rótulo |
|---|---|---|
| `porecatu_relay_hosts_registered` | gauge (lido do registro) | — |
| `porecatu_relay_hosts_registered_total` | counter (substituição conta) | — |
| `porecatu_relay_channels_open` | gauge | — |
| `porecatu_relay_channels_opened_total` | counter | — |
| `porecatu_relay_frames_total` | counter | `direction` (`client_to_host`, `host_to_client`) |
| `porecatu_relay_bytes_total` | counter (bytes de `payload`) | `direction` |
| `porecatu_relay_channel_closes_total` | counter | `reason` (os seis motivos da especificação e `other`) |
| `porecatu_relay_host_connection_closes_total` | counter | `code` (código de fechamento enviado, ou `none`) |

Nenhum rótulo com `host_id`, IP, nome de token ou canal; o teste `tests/metrics.rs` confere.

> **Na implementação:** não há histograma nenhum, e a latência de encaminhamento do [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.21 **não está implementada** — pendência aceita pelo dono em 2026-11-16 ([roadmap](roadmap.md#pendências)).

## 10. TLS

`tls::build_server_config` monta o `rustls::ServerConfig` com o provedor `ring` explícito, sem autenticação de cliente, ALPN só `http/1.1`, TLS 1.3 (`min_version = "1.3"`) ou 1.2 e 1.3 (`"1.2"`). A recarga (`tls::watch`, numa task) consulta a cada `[tls] reload_check_seconds` (piso 1) o `mtime` **e o tamanho** dos dois arquivos; mudando, carrega o par novo numa `spawn_blocking` e o troca com `RustlsConfig::reload_from_config`. Só as conexões seguintes o usam. Par inválido: mantém o anterior e loga `tls_reload_failed`, com o nome do arquivo e nunca o conteúdo.

## 11. Estratégia de teste

Medido em 2026-11-16 com `cargo test`: **238 testes**, todos passando — 153 de unidade na biblioteca, 79 de integração e 6 de documentação (5 deles `compile_fail`).

- **Unidade** (`#[cfg(test)]` de cada módulo): configuração (pisos, chaves desconhecidas, variáveis de ambiente, regra do TLS, tokens), adaptador do envelope, `host_id`, registro e alocador de canais, baldes e limitadores, IP do par, tokens, linha de comando, filtro de logs, métricas, TLS e recarga.
- **Compilação negativa**: os exemplos `compile_fail` de `src/payload.rs` exigem que `Debug`, `Display`, `tracing` com `?payload`, `Serialize` e `clone()` sobre `OpaquePayload` não compilem.
- **Integração**, em processo, com host e cliente falsos em `tokio-tungstenite` (`tests/common`):
  - `registration.rs` — token (`401`), registro (ok, assinatura errada, nonce errado, chave inválida, prazo, lixo, envelope errado, segundo `register`, canal nunca aberto, texto, quadro grande), substituição (`4002`), registro cheio (`4003`), keepalive, encerramento, presença, `/healthz`, limites por IP e `X-Forwarded-For`;
  - `routing.rs` — ciclo do canal, ordem de mil quadros em cada sentido e nos dois ao mesmo tempo, dois clientes no mesmo host, cada motivo de fechamento do host, host caído, substituição, keepalive do cliente;
  - `limits.rs` — cada limite do RF-1.14 estourado com o código certo, baldes por canal e por sentido, o cliente que não lê perdendo só o próprio canal, e a contrapressão da fila do host;
  - `blind_log.rs` — o log cego (§9);
  - `metrics.rs` — formato, contagens e ausência de rótulo de identidade;
  - `shutdown.rs` — `1001` e `closed { normal }`, e o prazo de encerramento contra um par que não responde;
  - `tls_reload.rs` — `wss://`, troca do par sem derrubar a conexão aberta, par inválido mantendo o anterior, encerramento sobre TLS;
  - `smoke.rs` — o binário sobe com o [relay.example.toml](reference/relay.example.toml) e responde `/healthz`; configuração inválida sai com `2` e nomeia a chave; porta ocupada sai com `1`.
- **Vetores de envelope**: rodam no repositório do protocolo, contra o `porecatu-protocol` que o relay usa; o relay não os roda de novo.
- **Carga** (etapa R3): mil hosts ociosos e cem canais ativos, medindo memória por canal e latência acrescentada contra as métricas do [PRD-000](prd/prd-000-visao-de-produto.md). **Não feito** ([roadmap](roadmap.md)).
