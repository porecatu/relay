# CLAUDE.md

Guia operacional do Porecatu Relay. Leia antes de mexer em qualquer coisa.

## O que é

**Porecatu Relay** — servidor auto-hospedado, cego e sem estado persistente, que encaminha a sessão cifrada entre o [Porecatu](https://github.com/porecatu/terminal) desktop (o host) e o app [Porecatu Remote](https://github.com/porecatu/remote) (o cliente). Implementa as camadas 1 e 2 do [protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md): WebSocket sobre TLS e o envelope CBOR de registro e roteamento. Nunca vê nada acima disso.

**Estado (2026-11-16):** implementação inicial feita, versão `0.1.0`, nada publicado. R0 a R2 do [roadmap](docs/roadmap.md) fechadas; R3 em andamento. **238 testes passando** no Windows (153 de unidade, 79 de integração, 6 de documentação). O que existe e foi verificado por teste automatizado:

- binário `porecatu-relay` (`axum` 0.8.9, `axum-server` com `rustls`/`ring`, `tungstenite` `=0.29.0`), `--version`, `--help`, `token new`;
- configuração por `relay.toml` + variáveis `PORECATU_RELAY_*`, com pisos da especificação e chave desconhecida como erro;
- registro do host (token do operador em tempo constante, desafio Ed25519 com `verify_strict`), substituição, presença, `/healthz`;
- roteamento cliente ↔ host em ordem, com contrapressão no sentido cliente → host e fechamento `limit-exceeded` no sentido host → cliente;
- limites por canal e por IP, `X-Forwarded-For` de proxy confiável;
- log cego (`tests/blind_log.rs`), métricas Prometheus escritas à mão, TLS embutido com recarga a quente, desligamento gracioso.

**Dívida de verificação:** o empacotamento (`Dockerfile`, `packaging/`, `.github/workflows/release.yml`) está escrito e **nunca rodou** — não há Docker, `musl` nem `systemd` nesta máquina, e não houve tag. O CI também nunca rodou. Testes só no Windows.

**Dependência do protocolo:** o `porecatu-protocol` é dependência git fixada num commit do `main` do protocolo (`rev = "a52fbb1…"`, a especificação 0.6); quando o crate for publicado com versão, trocar por versão exata. Mudança no protocolo que o relay precise: push no protocolo, depois subir o `rev` aqui e regerar o `Cargo.lock`. Para iterar localmente com a dependência git, `.cargo/config.toml` **não commitado** (já no `.gitignore`) com `[patch."https://github.com/porecatu/protocol"]` apontando para o caminho local. Outras pendências (métrica de latência, aviso de vencimento do certificado, aviso de `behind_proxy`): [roadmap, "Pendências"](docs/roadmap.md#pendências).

**A especificação do protocolo é normativa.** Divergência entre este repositório e [porecatu/protocol](https://github.com/porecatu/protocol) se resolve lá primeiro: o relay não inventa campo, código de fechamento nem limite. Links para a especificação são sempre URLs absolutas.

## Stack travada

Rust stable **pinada** em `rust-toolchain.toml` (`1.98.0`), edition 2024 ([ADR-0007](docs/adr/0007-toolchain-pinada.md)) · `tokio` (runtime) · `axum` com o WebSocket dele (`axum::extract::ws`) ([ADR-0001](docs/adr/0001-rust-tokio-e-axum.md)) · `rustls` com `ring` via `axum-server` ([ADR-0004](docs/adr/0004-tls-embutido-ou-atras-de-proxy.md)) · `ed25519-dalek` (verificação do registro) · `blake2` (resumo dos tokens; o `host_id` vem do `porecatu-protocol`) · `subtle` (comparação em tempo constante) · `tracing`/`tracing-subscriber` (logs) · `porecatu-protocol` (envelope e `host_id`), **versão exata** quando publicado — hoje por `rev` de git, ver "Estado".

Toda dependência direta tem versão exata (`=x.y.z`), menos, por ora, o `porecatu-protocol`. Cada escolha tem um ADR em [docs/adr/](docs/adr/0000-template.md). Não troque nenhuma sem ADR novo.

## Onde está cada coisa

Mapa dos módulos de `src/` na [arquitetura §0](docs/arquitetura.md). Em uma linha: `server` (rotas e estado compartilhado), `host` e `client` (uma task por conexão), `registry` (mapa de hosts e números de canal), `wire` (adaptador do envelope), `payload` (tipo opaco), `peer` (IP do par), `limits` (baldes e limites por IP), `config`, `token`, `tls`, `logging`, `metrics`, `shutdown`, `cli`. Testes de integração em `tests/`, com host e cliente falsos em `tests/common`.

## Comandos

```bash
cargo build
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo run -- --config docs/reference/relay.example.toml
python scripts/verify-docs.py      # links da documentação
```

Warning de clippy é erro. `unsafe_code = "deny"` sem exceção. Lints em `[lints]` do `Cargo.toml` (`unsafe_code = "deny"`, `missing_docs = "warn"`, `clippy::all = "warn"`), os mesmos do `[workspace.lints]` do Porecatu. `cargo deny` e `cargo audit` só no CI (não estão instalados localmente).

`cargo run -- --config docs/reference/relay.example.toml` sobe em `127.0.0.1:8080`, sem TLS, com um token de desenvolvimento. Código de saída: `0` ok, `1` falha em execução (porta ocupada), `2` uso ou configuração inválida.

## Convenções

- **Código, identificadores, comentários de código e nomes de arquivo de código: inglês.**
- **Documentação (README, ADR, PRD, docs/) e mensagens de commit: português do Brasil.**
- Commits em [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`, `ci:`. Commits com `Signed-off-by` ([CONTRIBUTING.md](CONTRIBUTING.md)).
- Arquivos de doc em `kebab-case.md`.
- Actions do GitHub pinadas por **SHA de commit**, com a versão no comentário.
- Todo arquivo de código-fonte e script começa com `// SPDX-License-Identifier: GPL-3.0-or-later` (ou `#` em Python e shell). Sem exceção.
- `LICENSE` é cópia verbatim da FSF e **nunca é editado** (sha256 `3972dc97…36986`, o mesmo do Porecatu).
- Licença de dependência nova: conferir compatibilidade com GPLv3 antes de adotar.

## Processo de decisão

Decisão aceita **não se edita**. Para mudar: ADR novo com o próximo número e `Supersedes: ADR-NNNN`; o antigo vira `Superseded by ADR-MMMM`; atualize o índice em [docs/adr/0000-template.md](docs/adr/0000-template.md) e, se for stack, a lista acima. Correção de erro factual no texto é permitida; mudança de decisão, não.

## Armadilhas

- **Nunca logar `payload`.** Nem em `debug`, nem truncado, nem em hex. O relay é cego por decisão ([ADR-0002](docs/adr/0002-relay-cego-e-sem-estado-persistente.md)); um log com conteúdo desfaz a decisão em silêncio. Log de conexão leva `host_id`, `channel`, motivo de fechamento e contagens — nada mais.
- **Token do operador comparado em tempo constante** (`subtle::ConstantTimeEq`), contra todos os tokens configurados, sem curto-circuito no primeiro que casa. E nunca aparece em log, nem mascarado.
- **Nunca descartar um `payload` em silêncio.** Um registro Noise perdido quebra o contador de nonce e a sessão inteira. Fila do canal (host → cliente) cheia, quadro grande demais ou banda estourada **fecham o canal** com `limit-exceeded`. A fila do host (cliente → host) cheia **não fecha nada**: aplica contrapressão — o cliente espera vaga e para de ser lido ([arquitetura §5](docs/arquitetura.md)).
- **Nenhum `Mutex` atravessa um `.await`.** Registro e limitadores por IP são `std::sync::Mutex` com métodos síncronos; prazos são `sleep_until` dentro do `select!` da task.
- **Toda escrita em socket passa por `server::send_bounded`** (prazo de 2 s): um par que não lê não pode segurar a task.
- **Ordem é contrato.** O relay entrega os `payload` de um canal na ordem em que chegaram. Nada de processar um canal em mais de uma task.
- **`channel` nunca é reaproveitado** durante a vida da conexão de um host, mesmo depois de fechado.
- **Limites só por configuração**, nunca constante solta: todo limite tem chave no `relay.toml`, com default igual ou acima do piso da especificação ([01 §6](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). `max_frame_bytes` é exatamente 70 000 (piso e teto). Os prazos de keepalive vêm do `porecatu-protocol`, não de chave.
- **O IP do par vale para logs e limites** (`peer::resolve_peer`): socket, ou o `X-Forwarded-For` mais à direita fora de `trusted_proxies` quando o socket é proxy confiável; cadeia toda confiável ou cabeçalho inválido dá o socket ([PRD-001](docs/prd/prd-001-operacao-do-relay.md) RF-1.13).
- **Sem disco.** Nenhuma tabela, arquivo ou cache sobrevive ao processo. Reiniciar o relay derruba todas as sessões, e está certo: host e cliente reconectam.
- **Sem TLS só em loopback** (ou com `behind_proxy = true` explícito): ver [ADR-0004](docs/adr/0004-tls-embutido-ou-atras-de-proxy.md).

## Índice

- [docs/prd/](docs/prd/prd-000-visao-de-produto.md) — visão e requisitos de operação
- [docs/arquitetura.md](docs/arquitetura.md) — tasks, registro de hosts, roteamento, contrapressão
- [docs/adr/](docs/adr/0000-template.md) — decisões
- [docs/reference/relay.example.toml](docs/reference/relay.example.toml) — configuração de referência
- [docs/roadmap.md](docs/roadmap.md) — etapas, dívida de verificação e pendências
- [CHANGELOG.md](CHANGELOG.md) — o que entrou em cada versão
