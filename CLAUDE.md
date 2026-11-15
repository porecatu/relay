# CLAUDE.md

Guia operacional do Porecatu Relay. Leia antes de mexer em qualquer coisa.

## O que é

**Porecatu Relay** — servidor auto-hospedado, cego e sem estado persistente, que encaminha a sessão cifrada entre o [Porecatu](https://github.com/porecatu/terminal) desktop (o host) e o app [Porecatu Remote](https://github.com/porecatu/remote) (o cliente). Implementa as camadas 1 e 2 do [protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md): WebSocket sobre TLS e o envelope CBOR de registro e roteamento. Nunca vê nada acima disso.

**Estado:** só documentação (etapa R0 do [roadmap](docs/roadmap.md)). Nenhum código.

**A especificação do protocolo é normativa.** Divergência entre este repositório e [porecatu/protocol](https://github.com/porecatu/protocol) se resolve lá primeiro: o relay não inventa campo, código de fechamento nem limite. Links para a especificação são sempre URLs absolutas.

## Stack travada

Rust stable **pinada** em `rust-toolchain.toml`, edition 2024 ([ADR-0007](docs/adr/0007-toolchain-pinada.md)) · `tokio` (runtime) · `axum` com o WebSocket dele (`axum::extract::ws`) ([ADR-0001](docs/adr/0001-rust-tokio-e-axum.md)) · `rustls` via `axum-server`/`tokio-rustls` ([ADR-0004](docs/adr/0004-tls-embutido-ou-atras-de-proxy.md)) · `ciborium` (CBOR) · `ed25519-dalek` (verificação do registro) · `blake2` (`host_id`) · `subtle` (comparação em tempo constante) · `porecatu-protocol` (tipos do envelope), **versão exata**.

Cada escolha tem um ADR em [docs/adr/](docs/adr/0000-template.md). Não troque nenhuma sem ADR novo.

## Comandos

```bash
cargo build
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo run -- --config docs/reference/relay.example.toml
python scripts/verify-docs.py      # links da documentação
```

Warning de clippy é erro. `unsafe_code = "deny"` sem exceção. Lints em `[lints]` do `Cargo.toml`, no mesmo espírito do `[workspace.lints]` do Porecatu.

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
- **Nunca descartar um `payload` em silêncio.** Um registro Noise perdido quebra o contador de nonce e a sessão inteira. Fila cheia, quadro grande demais ou banda estourada **fecham o canal** com `limit-exceeded`.
- **Ordem é contrato.** O relay entrega os `payload` de um canal na ordem em que chegaram. Nada de processar um canal em mais de uma task.
- **`channel` nunca é reaproveitado** durante a vida da conexão de um host, mesmo depois de fechado.
- **Limites só por configuração**, nunca constante solta: todo limite tem chave no `relay.toml`, com default igual ou acima do piso da especificação ([01 §6](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)).
- **Sem disco.** Nenhuma tabela, arquivo ou cache sobrevive ao processo. Reiniciar o relay derruba todas as sessões, e está certo: host e cliente reconectam.
- **Sem TLS só em loopback** (ou com `behind_proxy = true` explícito): ver [ADR-0004](docs/adr/0004-tls-embutido-ou-atras-de-proxy.md).

## Índice

- [docs/prd/](docs/prd/prd-000-visao-de-produto.md) — visão e requisitos de operação
- [docs/arquitetura.md](docs/arquitetura.md) — tasks, registro de hosts, roteamento, contrapressão
- [docs/adr/](docs/adr/0000-template.md) — decisões
- [docs/reference/relay.example.toml](docs/reference/relay.example.toml) — configuração de referência
- [docs/roadmap.md](docs/roadmap.md) — etapas
