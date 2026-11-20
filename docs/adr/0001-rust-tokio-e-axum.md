# ADR-0001 — Rust, `tokio` e `axum`

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0004](0004-tls-embutido-ou-atras-de-proxy.md), [ADR-0007](0007-toolchain-pinada.md), [PRD-000](../prd/prd-000-visao-de-produto.md)

## Contexto

O relay é um servidor de rede de vida longa que mantém muitas conexões WebSocket abertas e quase ociosas (um host conectado o dia inteiro, clientes que entram e saem), encaminhando quadros binários sem interpretá-los. O que ele precisa:

- concorrência barata por conexão — milhares de conexões ociosas não podem custar milhares de threads;
- WebSocket servidor maduro, com `Ping`/`Pong`, limite de tamanho de quadro e códigos de fechamento próprios ([especificação §1 e §5](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md));
- TLS sem OpenSSL, para um binário estático;
- o mesmo crate de tipos do envelope que o Porecatu desktop usa (`porecatu-protocol`), para os dois lados nunca divergirem na codificação CBOR.

O dono do produto decidiu pela stack Rust antes da escrita. Este ADR registra o recorte dentro dela.

## Decisão

**O relay é um binário Rust sobre `tokio`, com o servidor HTTP e WebSocket do `axum` (`axum::extract::ws`), TLS por `rustls`, e os tipos do envelope do crate `porecatu-protocol`.**

- **`tokio`** com runtime multi-thread, uma task por conexão WebSocket (host ou cliente) e canais `tokio::sync::mpsc` **limitados** entre elas ([arquitetura](../arquitetura.md)).
- **`axum`** para o roteamento HTTP (`/v0/host`, `/v0/client/{host_id}`, `/v0/presence/{host_id}`, `/healthz`) e o upgrade de WebSocket. O extrator de cabeçalho dá o `Authorization` antes do upgrade, que é onde a recusa com HTTP 401 tem de acontecer.
- **`rustls`** com o provedor criptográfico `ring` (o mesmo do `russh` no Porecatu), via `axum-server`; ver [ADR-0004](0004-tls-embutido-ou-atras-de-proxy.md).
- **`ciborium`** para CBOR, **`ed25519-dalek`** para verificar o registro, **`blake2`** para o `host_id`, **`subtle`** para comparar tokens.
- **`porecatu-protocol`** pinado por **versão exata** (`=x.y.z`): os tipos do envelope e o cálculo do `host_id` vêm de lá, testados contra os vetores do repositório do protocolo.
- **`tracing`** para logs estruturados, com um filtro que não deixa campo de conteúdo existir (ver [ADR-0002](0002-relay-cego-e-sem-estado-persistente.md)).

> **Nota de implementação (2026-11-16):** a decisão se manteve; três detalhes saíram diferentes do texto acima.
>
> - **`ciborium` não é dependência direta do relay.** Toda codificação CBOR do envelope está no `porecatu-protocol` (que usa `ciborium`); o relay só chama `porecatu_protocol::envelope` pelo adaptador `src/wire.rs`.
> - **`porecatu-protocol` ainda não está pinado por versão exata.** Entra como dependência git fixada num commit do `main` do protocolo (`rev = "4ce7e4f…"` desde 2026-11-20, a especificação 0.5); vira versão exata quando o crate for publicado ([roadmap](../roadmap.md#pendências)).
> - O `tungstenite` que o `axum` resolve (`=0.29.0`) é dependência direta, só para reconhecer o erro de capacidade (quadro grande demais, `1009`) por tipo, não pelo texto da mensagem.

## Alternativas consideradas

- **TypeScript (Node ou Bun).** Compartilharia a implementação TS do protocolo com o app Expo. Rejeitada pelo dono; e o relay só precisa do envelope, que é a parte mais simples do protocolo — o ganho de compartilhar com o app seria pequeno, e o de compartilhar com o desktop (Rust) é maior, porque é o desktop quem gera os envelopes de host.
- **Go.** Concorrência barata e `nhooyr/websocket` maduro. Rejeitada: uma terceira linguagem no ecossistema sem ganho que Rust não dê, e sem compartilhar o crate de protocolo.
- **`tokio-tungstenite` direto, sem `axum`.** Menos uma dependência. Rejeitada: o relay ainda precisa de roteamento HTTP (`/healthz`, dois caminhos com parâmetro, recusa 401 antes do upgrade) e de `/metrics` opcional; reescrever isso à mão é código de servidor HTTP que o `axum` já testa. O `axum` usa `tokio-tungstenite` por baixo.
- **`actix-web`.** Igualmente capaz. Rejeitada por ter runtime próprio sobre o `tokio` e um modelo de atores que não ajuda num servidor que é só um roteador de bytes.
- **`hyper` puro.** Controle máximo, código máximo. Rejeitada pelo mesmo motivo do `tokio-tungstenite` direto.

## Consequências

### Positivas

- Um binário estático (`musl`) sem dependência de sistema, fácil de empacotar ([ADR-0005](0005-empacotamento-binario-e-docker.md)).
- Os mesmos tipos de envelope do desktop: uma divergência de codificação aparece como teste quebrado no crate de protocolo, não como sessão que não abre em produção.
- Disciplina do Porecatu (toolchain pinada, clippy como erro, `unsafe_code = "deny"`) vale aqui sem adaptação.

### Negativas

- `axum` e `tokio` sobem com frequência; cada subida é tarefa própria, com changelog lido.
- O provedor `ring` do `rustls` precisa de compilador C no build para algumas arquiteturas.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Mudança incompatível do `axum` no WebSocket | média | baixo | Versão exata no `Cargo.lock`, subida como tarefa própria com os testes de integração do roteamento |
| `porecatu-protocol` e relay em versões que codificam diferente | baixa | alto | Versão exata; o CI roda os vetores de envelope do protocolo contra o relay |

> **Nota de implementação (2026-11-16):** os vetores de envelope rodam hoje no repositório do protocolo, contra o mesmo `porecatu-protocol` que o relay usa; o CI do relay não os roda de novo. A versão exata depende da publicação do protocolo (nota acima).
| Vulnerabilidade numa dependência de rede | média | alto | `cargo deny` e `cargo audit` no CI; Dependabot com SHA pinado |
