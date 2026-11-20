<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Changelog

Todas as mudanças relevantes do Porecatu Relay ficam aqui. O formato segue o
[Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/), e o projeto segue
o [Versionamento Semântico](https://semver.org/lang/pt-BR/).

Uma versão que passe a servir um envelope novo (`/v1/`) continua servindo o
anterior por pelo menos uma versão menor, e este arquivo avisa quando o antigo
sai (PRD-001, RF-1.29). Quem usa a imagem `latest` deve ler este arquivo antes
de atualizar; fixar `X.Y` evita surpresas.

## [Não lançado]

Implementação inicial, versão `0.1.0` no `Cargo.toml`. Serve o envelope
`/v0/` da [especificação do protocolo](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md).

### Mudado

- `porecatu-protocol` sobe para o commit da especificação 0.5 (`4ce7e4f`, 2026-11-20), que troca a ação `activate-tab` pela mensagem `view-tab`. O relay não lê mensagens de aplicação: nada muda no comportamento; o `rev` sobe para os três projetos falarem o mesmo commit.

### Adicionado

- Relay cego entre o host (desktop) e os clientes (celulares): registro do host
  por chave e token do operador, roteamento de `payload` opaco por canal, sem
  contas e sem nada em disco.
- Registro do host em `/v0/host`: token do operador no `Authorization`,
  comparado em tempo constante contra todos os configurados (`401` antes do
  upgrade), desafio Ed25519 verificado com `verify_strict` (`4001`), prazo de
  registro (`4005`), registro cheio (`4003`) e substituição pelo registro mais
  novo do mesmo `host_id` (`4002`, clientes com `closed { host-replaced }`).
- Canais em `/v0/client/{host_id}`: `host-unavailable` sem host, números de
  canal monotônicos e nunca reaproveitados, ordem garantida nos dois sentidos,
  contrapressão no sentido cliente → host e fechamento `limit-exceeded` do
  canal no sentido host → cliente quando o celular não acompanha.
- Presença em `/v0/presence/{host_id}` (`204`/`404`/`400`/`429`) e `/healthz`.
- Keepalive: `Ping` a cada 25 s, conexão derrubada sem `Pong` em 60 s.
- Configuração por `relay.toml` e por variáveis `PORECATU_RELAY_*` (exceto
  tokens), com limites nunca abaixo do piso da especificação; chave
  desconhecida, nenhum token ou TLS ausente fora de loopback (sem
  `behind_proxy`) impedem a subida, com arquivo, linha e chave na mensagem.
- Limites configuráveis: canais por host, tamanho de quadro (70 000 bytes),
  banda e rajada por canal e sentido, hosts no total, fila por canal e por
  host, prazo de registro, e conexões de cliente, tentativas de host e
  consultas de presença por IP por minuto (`429`).
- IP do par pelo socket ou pelo `X-Forwarded-For` de proxy em
  `[server] trusted_proxies`, nos logs e nos limites; IPv6 agrupado por `/64`
  nos limites.
- TLS embutido (`rustls` com `ring`, TLS 1.3 por padrão, 1.2 opcional) com
  recarga do certificado sem derrubar conexões abertas, ou atrás de proxy
  reverso.
- Logs estruturados em `stderr`, texto ou JSON, sem `payload`, token ou
  cabeçalho `Authorization` em nível nenhum, com o IP completo, truncado ou
  omitido (`[log] ip_addresses`).
- Métricas Prometheus em `GET /metrics`, num listener separado e desligadas por
  padrão, só com contagens agregadas.
- Desligamento gracioso em `SIGINT`/`SIGTERM` (Ctrl+C, fechamento do console e
  desligamento do sistema no Windows), limitado por
  `[server] shutdown_grace_seconds`.
- `porecatu-relay token new` para gerar tokens do operador; `--version` com a
  versão do envelope servido.
- Binários estáticos para Linux (x86_64 e aarch64, `musl`), macOS e Windows,
  com `.sha256` ao lado, e imagem de contêiner multi-arquitetura em
  `ghcr.io/porecatu/relay` (distroless, usuário não-root, porta 8080), pelo
  workflow de release. **Ainda não exercitados.**
- Unidade `systemd` de exemplo, com endurecimento, em
  `packaging/porecatu-relay.service`. **Ainda não exercitada.**

### Pendente

- Métrica de latência de encaminhamento (PRD-001 RF-1.21) e aviso de
  vencimento do certificado a 14 dias (ADR-0004), aceitos como pendência.
- O crate `porecatu-protocol` entra como dependência git fixada num commit
  do protocolo até ser publicado com versão.
