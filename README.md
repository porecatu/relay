# Porecatu Relay

Servidor **auto-hospedado** que liga o [Porecatu](https://github.com/porecatu/terminal) desktop ao app [Porecatu Remote](https://github.com/porecatu/remote), para acessar do celular uma sessão viva do Porecatu.

**Estado:** implementação inicial pronta e testada (versão `0.1.0`), **ainda sem release**. O relay registra hosts, roteia canais, aplica os limites, termina TLS com recarga do certificado e expõe métricas; 238 testes automatizados passam. Ainda não verificados: a imagem de contêiner, os binários `musl`, a unidade `systemd` e o workflow de release, que estão escritos mas nunca rodaram. Detalhe e pendências no [roadmap](docs/roadmap.md).

## O que ele é

- **Cego.** Encaminha bytes cifrados de ponta a ponta entre o desktop e o celular. Não tem as chaves da sessão, não lê terminais, teclas, títulos nem diretórios.
- **Sem contas.** Quem pode registrar um desktop é quem tem um **token do operador**, definido no arquivo de configuração. A identidade do desktop é uma chave; a do celular também, e quem confere a do celular é o próprio desktop.
- **Sem persistência.** Nada vai para disco além do que o operador escolher logar — e os logs nunca têm conteúdo.
- **Auto-hospedado.** Não há relay público oficial. Você roda o seu: um VPS, um servidor de casa, um contêiner atrás do proxy reverso que você já usa.

```
Porecatu desktop  ──wss──▶  Porecatu Relay  ◀──wss──  Porecatu Remote
        └──────────── sessão Noise ponta a ponta ────────────┘
```

O protocolo que os três falam é especificado em [porecatu/protocol](https://github.com/porecatu/protocol/blob/main/docs/especificacao/00-visao-geral.md). Este repositório implementa só a parte do relay: [transporte e envelope](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md).

## Início rápido

> Ainda não há release: os binários e a imagem `ghcr.io/porecatu/relay` só existem depois da primeira tag. Por enquanto, compile a partir do código. O crate do protocolo vem do repositório [porecatu/protocol](https://github.com/porecatu/protocol), fixado num commit, e o `cargo` o baixa sozinho.

```bash
# Desenvolvimento local: sobe em 127.0.0.1:8080, sem TLS, com um token de exemplo
cargo run -- --config docs/reference/relay.example.toml

# 1. Gere um token para o seu desktop (imprime o bloco [[tokens]] pronto para colar)
porecatu-relay token new --name notebook

# 2. Rode com TLS embutido ([tls] cert_file e key_file no relay.toml)
porecatu-relay --config /etc/porecatu-relay/relay.toml
```

Em contêiner, atrás do seu proxy reverso (depois da primeira release; não verificado ainda): a imagem escuta em 8080 sem TLS, com `behind_proxy = true`, e lê o token de `/run/secrets/porecatu-relay-token`.

```bash
docker run -p 127.0.0.1:8080:8080 \
  -v ./token:/run/secrets/porecatu-relay-token:ro \
  ghcr.io/porecatu/relay
```

Para o relay ver o IP real dos celulares atrás do proxy, informe o endereço do proxy em `PORECATU_RELAY_SERVER_TRUSTED_PROXIES` (ou em `[server] trusted_proxies`). Há uma unidade `systemd` de exemplo, endurecida, em [packaging/porecatu-relay.service](packaging/porecatu-relay.service).

Sem `--config`, o relay procura `/etc/porecatu-relay/relay.toml` no Linux e `relay.toml` ao lado do executável nas outras plataformas. Configuração inválida impede a subida, com o arquivo, a linha e a chave na mensagem. Mudar a configuração é reiniciar; só o certificado TLS é relido sem reiniciar.

No Porecatu desktop, em **Configurações → Acesso remoto**, informe a URL do relay (`wss://relay.seudominio.com.br`) e o token. O pareamento do celular é por QR code, mostrado pelo desktop; o relay não participa dele.

A configuração completa, comentada, está em [docs/reference/relay.example.toml](docs/reference/relay.example.toml). Variáveis `PORECATU_RELAY_<SEÇÃO>_<CHAVE>` sobrescrevem chaves, exceto os tokens, que só vêm do arquivo (literal ou `value_file`).

## Observação

- `GET /healthz` responde `200 ok`, sem autenticação.
- `[metrics] enabled = true` liga `GET /metrics` (formato Prometheus) num endereço separado (`[metrics] bind`, padrão `127.0.0.1:9464`). Só contagens agregadas, sem `host_id`, IP ou nome de token.
- Logs em `stderr`, texto ou JSON, sem conteúdo de sessão, token ou cabeçalho `Authorization` em nível nenhum; o IP aparece truncado por padrão (`[log] ip_addresses`).

## Documentação

- [Visão de produto](docs/prd/prd-000-visao-de-produto.md) e [operação do relay](docs/prd/prd-001-operacao-do-relay.md)
- [Arquitetura](docs/arquitetura.md)
- [Decisões (ADRs)](docs/adr/0000-template.md)
- [Roadmap](docs/roadmap.md)
- [CHANGELOG](CHANGELOG.md)
- [Modelo de ameaça do protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/modelo-de-ameaca.md)

## Projetos relacionados

| Projeto | Papel |
|---|---|
| [porecatu/terminal](https://github.com/porecatu/terminal) | O emulador de terminal; o **host** da sessão remota |
| [porecatu/protocol](https://github.com/porecatu/protocol) | Especificação do protocolo, vetores de teste e implementações de referência |
| [porecatu/remote](https://github.com/porecatu/remote) | O app mobile (Android e iOS) |

## Licença

[GPL-3.0-or-later](LICENSE).
