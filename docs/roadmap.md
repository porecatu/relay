# Roadmap

Etapas de entrega do Porecatu Relay. Cada etapa tem **critério de saída explícito**; enquanto ele não for atendido, a etapa não terminou e a próxima não começa.

O relay anda junto com os outros dois lados do acesso remoto: o [Porecatu desktop](https://github.com/porecatu/terminal) (host) e o [Porecatu Remote](https://github.com/porecatu/remote) (cliente), sobre a [especificação do protocolo](https://github.com/porecatu/protocol). A primeira versão publicada sai **junto** com a primeira versão de fio `[0, 1]` do protocolo.

| Etapa | Status |
|---|---|
| R0 — Documentação | **fechada** |
| R1 — Envelope, registro e roteamento | **fechada** (dívida de verificação, ver seção dela) |
| R2 — TLS, limites e configuração | **fechada** (duas pendências aceitas pelo dono, ver seção dela) |
| R3 — Empacotamento e operação | **em andamento** (empacotamento escrito e não verificado; guia de operação e teste de carga por fazer) |
| R4 — Primeira versão | planejada |

> **Medido em 2026-11-16**, no Windows, com `cargo test`: **238 testes passando** — 153 de unidade, 79 de integração (`tests/`) e 6 de documentação (5 `compile_fail`). O CI ainda não rodou no GitHub.

---

## R0 — Documentação

Visão, requisitos de operação, arquitetura e as decisões de stack, cegueira, registro, TLS, empacotamento, licença e toolchain.

**Critério de saída:**

1. PRD-000, PRD-001, arquitetura e ADR-0001 a 0007 escritos e revisados pelo dono.
2. `python scripts/verify-docs.py` passa.
3. Nenhuma contradição com a [especificação, §1 a §7](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md): onde houver dúvida, ela foi levada ao repositório do protocolo, não resolvida aqui.

**Fechada.** Depois da implementação, a arquitetura, o PRD-001 e os ADRs foram reconciliados com o código em 2026-11-16 (notas de implementação datadas em cada um).

## R1 — Envelope, registro e roteamento

Depende do crate `porecatu-protocol` com os tipos do envelope e o `host_id` (etapa correspondente do roadmap do protocolo).

- Esqueleto: `Cargo.toml` com lints, `rust-toolchain.toml`, CI nas três plataformas, `cargo deny`.
- `/v0/host` com token, desafio e registro; `/v0/client/{host_id}`; `/v0/presence/{host_id}`; `/healthz`.
- Registro em memória com geração; tabela de canais na host task; filas limitadas nos dois sentidos com a política da [arquitetura §5](arquitetura.md).
- Substituição de host, keepalive, encerramento.
- Tipo opaco do `payload` sem `Debug`, com o teste de compilação negativa.

**Critério de saída:**

1. Os testes de integração da [arquitetura §11](arquitetura.md) passam, em HTTP simples (loopback).
2. Os vetores de envelope do protocolo passam.
3. O teste de log cego passa em `trace`.
4. Um host e um cliente de teste trocam mil quadros em ordem.

**Fechada.** Tudo da lista acima está no código (`src/server.rs`, `host.rs`, `client.rs`, `registry.rs`, `wire.rs`, `payload.rs`, `shutdown.rs`). Critérios 1, 3 e 4: `tests/registration.rs`, `tests/routing.rs` (mil quadros em cada sentido e nos dois ao mesmo tempo), `tests/blind_log.rs`, `tests/shutdown.rs`. A fila do sentido cliente → host saiu com **contrapressão**, sem fechar canal ([arquitetura §5](arquitetura.md)).

**Dívida de verificação:**

- Critério 2: os vetores de envelope rodam no repositório do protocolo, contra o mesmo `porecatu-protocol` que o relay usa; o relay não os roda de novo (nota no [ADR-0001](adr/0001-rust-tokio-e-axum.md)).
- O CI (`.github/workflows/ci.yml`: três plataformas, `cargo deny`, `cargo audit`, documentação) está escrito e **ainda não rodou**. `cargo deny` e `cargo audit` não rodaram nem localmente.
- Testes só no Windows; Linux e macOS só pelo CI, quando ele rodar.

## R2 — TLS, limites e configuração

- `relay.toml` completo, com validação, pisos da especificação e variáveis de ambiente ([PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.1 a RF-1.9).
- TLS embutido com recarga de certificado; regra de subida sem TLS ([ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md)).
- Banda por canal, limites por IP, `X-Forwarded-For` de proxy confiável.
- Logs em texto e JSON, política de IP, métricas opcionais.

**Critério de saída:**

1. Cada limite do RF-1.14 tem teste que o estoura e confere o código.
2. Configuração inválida (chave desconhecida, abaixo do piso, TLS ausente fora de loopback) impede a subida com mensagem que aponta a chave.
3. Troca de certificado no disco é aplicada sem derrubar conexão aberta.

**Fechada.** Critério 1: `tests/limits.rs` e `tests/registration.rs`. Critério 2: testes de `src/config` e `tests/smoke.rs` (o binário sai com código `2` e nomeia a chave). Critério 3: `tests/tls_reload.rs`. Configuração: `src/config/`; TLS: `src/tls.rs`; limites: `src/limits/`; IP do par: `src/peer.rs` (regra no [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.13, com a decisão do dono de 2026-11-16); logs: `src/logging.rs`; métricas: `src/metrics.rs`, texto Prometheus escrito à mão.

**Pendências aceitas pelo dono em 2026-11-16** (não impedem o fechamento; detalhe em [pendências](#pendências)): a métrica de latência de encaminhamento (RF-1.21) e o aviso de vencimento do certificado a 14 dias ([ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md)).

**Dívida de verificação:** TLS testado só com certificado gerado no teste (`rcgen`), em loopback; nunca com um certificado do Let's Encrypt renovado pelo `certbot`, nem atrás de proxy reverso de verdade.

## R3 — Empacotamento e operação

- Binários `musl` e das outras plataformas, imagem de contêiner multi-arquitetura, unidade `systemd` ([ADR-0005](adr/0005-empacotamento-binario-e-docker.md)).
- Guia de operação: VPS com `certbot`, atrás de Caddy, atrás de nginx, Docker Compose, Raspberry Pi.
- `porecatu-relay token new`.
- Teste de carga contra as métricas do [PRD-000](prd/prd-000-visao-de-produto.md).

**Critério de saída:**

1. Alguém que não escreveu o relay o sobe num VPS seguindo só o README em menos de 15 minutos.
2. Memória por canal ocioso e latência acrescentada dentro do alvo, medidas e registradas.

**Em andamento.** Entregue:

- `porecatu-relay token new` (`src/cli.rs`, `src/token.rs`), com teste.
- `Dockerfile`, `packaging/docker/relay.toml`, `packaging/porecatu-relay.service` e `.github/workflows/release.yml` escritos ([nota no ADR-0005](adr/0005-empacotamento-binario-e-docker.md)).

**Dívida de verificação** do que foi entregue: **nada do empacotamento foi exercitado.** Nesta máquina não há Docker, alvo `musl` nem `systemd`, e não houve tag: a imagem, os binários `musl`, o `.sha256`, a unidade `systemd` e o workflow de release nunca rodaram.

Por fazer: o guia de operação (nenhum dos cinco cenários está escrito), o teste de carga e os dois critérios de saída.

## R4 — Primeira versão

**Critério de saída:**

1. O protocolo congelou a versão de fio `[0, 1]` ([versionamento](https://github.com/porecatu/protocol/blob/main/docs/especificacao/04-versionamento.md)).
2. Um Porecatu desktop e um Porecatu Remote de verdade conversam através do relay, num VPS, com TLS.
3. Release publicada com binários, `sha256`, imagem e CHANGELOG.

### Dívida de verificação prevista

Como no Porecatu, o que não for exercitado de verdade fica escrito aqui, como dívida, quando a etapa fechar — não como critério que impede a seguinte. Já se sabe que: macOS e Windows são só alvo de teste local; e operação atrás de proxies que não sejam Caddy e nginx não será verificada.

---

## Pendências

O que o código ainda não faz, ou faz diferente do que os documentos pediam, com o estado da decisão. Conferido no código em 2026-11-16.

| Pendência | Estado | Onde |
|---|---|---|
| **`porecatu-protocol` por commit**: dependência git com `rev = "4ce7e4f…"` do `main` do protocolo (especificação 0.5, desde 2026-11-20; de 2026-11-17 até então, `0909e7c…`, a 0.4; de 2026-11-16 a 2026-11-17, `758d68c…`, a 0.3; antes, por caminho local). Trocar por versão exata quando o crate for publicado | Pendente, não bloqueia | [ADR-0001](adr/0001-rust-tokio-e-axum.md), [ADR-0007](adr/0007-toolchain-pinada.md) |
| **Métrica de latência de encaminhamento** (RF-1.21). Não implementada; não há histograma nenhum | Pendente, aceita pelo dono em 2026-11-16 | [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.21 |
| **Aviso de vencimento do certificado a 14 dias**, uma vez por dia. Não implementado | Pendente, aceita pelo dono em 2026-11-16 | [ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md), "Riscos" |
| **Aviso na subida com `behind_proxy = true` e `bind` fora de rede privada** (RFC 1918/4193). Não implementado | Registrada, sem decisão | [ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md), "Riscos" |
| **Configuração só muda por reinício.** Confirmado no código: só o certificado TLS é relido a quente | Registrada, sem decisão (é o que o RF-1.4 pede para o v0) | [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.4 |
| **`HostError::close_reason` devolve `normal`** para falhas de uma sessão já aberta (registro Noise, mensagem ou `pair-request` inválidos), e `host-rejected` só para falhas do handshake. Está no `porecatu-protocol` (`session/host.rs`), não no relay: o relay só repassa ao cliente o motivo do `close-channel` que o host mandar | Registrada, sem decisão; se mudar, muda no protocolo | [porecatu/protocol](https://github.com/porecatu/protocol) |
| **Caminho padrão da configuração fora do Linux**: ao lado do executável, não no diretório corrente | Doc alinhada ao código | [PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.1 |
| **Guia de operação** (VPS com `certbot`, Caddy, nginx, Docker Compose, Raspberry Pi) | Por fazer (R3) | [R3](#r3--empacotamento-e-operação) |
| **Teste de carga** e medição das métricas do PRD-000 | Por fazer (R3) | [PRD-000](prd/prd-000-visao-de-produto.md) |

---

## Fora do v0

Registrado para não ser reinventado como ideia nova. Justificativas no [PRD-000](prd/prd-000-visao-de-produto.md).

| Item | Onde está justificado |
|---|---|
| Relay público oficial | PRD-000, "Não-objetivos" |
| Contas de usuário | PRD-000 |
| Persistência e fila para cliente offline | PRD-000, [ADR-0002](adr/0002-relay-cego-e-sem-estado-persistente.md) |
| Notificação push | PRD-000 |
| ACME embutido | [ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md) |
| Pacotes `.deb`/`.rpm` | [ADR-0005](adr/0005-empacotamento-binario-e-docker.md) |
| Painel e API de administração | [PRD-001](prd/prd-001-operacao-do-relay.md), "Fora de escopo" |
| Recarga a quente da configuração | PRD-001 RF-1.4 |
