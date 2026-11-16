# ADR-0005 — Empacotamento: binário estático e imagem de contêiner

**Status:** Aceito
**Data:** 2026-11-15
**Relacionados:** [ADR-0001](0001-rust-tokio-e-axum.md), [ADR-0004](0004-tls-embutido-ou-atras-de-proxy.md), [ADR-0006](0006-licenca-gpl.md), [PRD-001](../prd/prd-001-operacao-do-relay.md)

## Contexto

O relay roda no servidor do usuário: VPS Linux na maioria dos casos, às vezes um Raspberry Pi (aarch64), às vezes um cluster de contêineres em casa. O [PRD-000](../prd/prd-000-visao-de-produto.md) tem como métrica ir do zero ao primeiro host registrado em menos de 15 minutos para quem já tem domínio e certificado. O empacotamento é o que mais pesa nisso.

## Decisão

**Cada versão publica binários estáticos por plataforma, com `sha256` ao lado, e uma imagem de contêiner multi-arquitetura. Não há pacote de distribuição (`.deb`, `.rpm`, Homebrew) no v0.**

### 1. Binários

| Alvo | Observação |
|---|---|
| `x86_64-unknown-linux-musl` | Estático, o caso principal |
| `aarch64-unknown-linux-musl` | Raspberry Pi 4/5, instâncias ARM |
| `x86_64-apple-darwin`, `aarch64-apple-darwin` | Para teste local e quem roda em Mac mini |
| `x86_64-pc-windows-msvc` | Para teste local |

- Publicados na release do GitHub, como `.tar.gz` (`.zip` no Windows), cada um com um `.sha256` ao lado. Junto: `LICENSE`, `README.md`, `relay.example.toml` e a unidade `systemd` de exemplo.
- Compilados no CI a partir da tag, com a toolchain pinada ([ADR-0007](0007-toolchain-pinada.md)).
- Sem assinatura de código no v0, como no Porecatu.

### 2. Imagem de contêiner

- Publicada em `ghcr.io/porecatu/relay`, com as tags `X.Y.Z`, `X.Y` e `latest`, multi-arquitetura (`linux/amd64`, `linux/arm64`).
- Construída a partir do binário `musl` já compilado, sobre **`gcr.io/distroless/static`** (ou `scratch` com certificados de CA): sem shell, sem gerenciador de pacotes.
- Roda como usuário não-root (`65532`). Porta padrão **8080** sem TLS dentro do contêiner, com `behind_proxy = true` no arquivo padrão da imagem, porque o caso típico é atrás de proxy ([ADR-0004](0004-tls-embutido-ou-atras-de-proxy.md)). TLS embutido continua disponível montando certificado e configuração.
- Configuração em `/etc/porecatu-relay/relay.toml` ou por variáveis de ambiente ([PRD-001](../prd/prd-001-operacao-do-relay.md) RF-1.3); tokens preferencialmente por `value_file` com segredo montado.
- `HEALTHCHECK` não é declarado na imagem (distroless não tem `curl`); o `/healthz` é para o orquestrador consultar de fora.

### 3. systemd

Unidade de exemplo em `packaging/porecatu-relay.service`, com `DynamicUser=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp=yes`, `NoNewPrivileges=yes`, `LoadCredential=` para os tokens, e `AmbientCapabilities=CAP_NET_BIND_SERVICE` comentada (só para escutar na 443 direto).

> **Nota de implementação (2026-11-16):** os três estão escritos e **nenhum foi exercitado** — não há Docker, `musl` nem `systemd` na máquina de desenvolvimento, e ainda não houve tag (dívida de verificação no [roadmap](../roadmap.md)).
>
> - **Binários:** `.github/workflows/release.yml`, numa tag `v*` que tem de bater com a versão do `Cargo.toml`. Os alvos `musl` compilam nativamente (x86_64 num runner x86_64, aarch64 num runner arm64) com `musl-gcc`; o pacote leva o binário, `LICENSE`, `README.md`, `relay.example.toml` e a unidade `systemd`, com o `.sha256` ao lado. As notas da release saem da seção da versão no `CHANGELOG.md`.
> - **Imagem:** o `Dockerfile` tem dois caminhos para o mesmo resultado: o alvo padrão compila num estágio `rust:1.98.0-alpine` (musl, estático); o alvo `runtime-prebuilt`, usado pela release, copia os binários `musl` já compilados, sem emulação. Os dois terminam em `gcr.io/distroless/static:nonroot` (imagens base fixadas por digest), usuário `65532`, porta 8080, com `packaging/docker/relay.toml` (`behind_proxy = true`, token por `value_file` em `/run/secrets/porecatu-relay-token`). Tags `X.Y.Z` e `X.Y` explícitas; `latest` vem do padrão do `docker/metadata-action` e não sai em pré-lançamento.
> - **systemd:** além do listado acima, a unidade zera o `CapabilityBoundingSet=` por padrão e traz comentadas as duas linhas que repõem só `CAP_NET_BIND_SERVICE`; o token chega por `LoadCredential=token:/etc/porecatu-relay/token`.
> - **Dependência do protocolo:** o `porecatu-protocol` vem do GitHub fixado num commit (`rev`), então o build da imagem e o da release baixam o crate sozinhos; nenhum dos dois rodou ainda ([roadmap](../roadmap.md#pendências)).

## Alternativas consideradas

- **Pacotes `.deb`/`.rpm`.** Instalação mais "nativa". Rejeitada no v0: o binário estático com a unidade `systemd` de exemplo cobre o mesmo caso com menos manutenção; pode entrar depois, com ADR.
- **Imagem baseada em Alpine ou Debian slim.** Teria shell para depuração. Rejeitada: o relay não precisa de nada do sistema, e um contêiner sem shell é uma superfície a menos.
- **Binário `glibc`.** Menor e um pouco mais rápido. Rejeitada para Linux: depende da versão da `glibc` do servidor, e o caso de VPS antigo é comum.
- **Só imagem de contêiner.** Rejeitada: o operador de Raspberry Pi sem Docker ficaria sem caminho.

## Consequências

### Positivas

- Um arquivo para baixar e rodar; nenhuma dependência de sistema.
- A imagem é pequena (alguns MB) e sem shell.
- O mesmo binário serve o contêiner e a instalação direta.

### Negativas

- `musl` tem alocador mais lento; o relay é limitado por rede, não por alocação, mas vale medir na etapa R3.
- Sem pacote de distribuição, atualizar é manual (trocar o binário) ou por imagem.

### Riscos e mitigação

| Risco | Probabilidade | Impacto | Mitigação |
|---|---|---|---|
| Alocador do `musl` pesa sob carga | baixa | baixo | Medir na R3; trocar o alocador global (`mimalloc`) é decisão pequena, com ADR |
| Imagem `latest` atualiza sem o operador saber | média | médio | Guia recomenda fixar `X.Y`; CHANGELOG marca mudança de envelope |
| Binário adulterado no download | baixa | alto | `sha256` publicado; imagem com digest; assinatura entra quando o Porecatu a adotar |
