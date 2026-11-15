# PRD-001 — Operação do relay

**Status:** Aprovado
**Data:** 2026-11-15
**Relacionados:** [PRD-000](prd-000-visao-de-produto.md), [ADR-0002](../adr/0002-relay-cego-e-sem-estado-persistente.md), [ADR-0003](../adr/0003-registro-do-host-por-chave-e-token-do-operador.md), [ADR-0004](../adr/0004-tls-embutido-ou-atras-de-proxy.md), [ADR-0005](../adr/0005-empacotamento-binario-e-docker.md), [especificação: transporte e relay](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)

## Problema

O relay é software que o próprio usuário opera. Tudo que ele precisa saber para subir, proteger, observar e atualizar o serviço tem de estar num arquivo de configuração legível e num punhado de comandos — sem painel, sem banco, sem passo manual escondido.

## Requisitos funcionais

### Configuração

- **RF-1.1** O relay lê **um arquivo TOML** (`relay.toml`), passado por `--config <caminho>`. Sem `--config`, procura `/etc/porecatu-relay/relay.toml` no Linux e `relay.toml` no diretório corrente nas outras plataformas. Arquivo ausente ou inválido: o processo **não sobe**, e a mensagem diz o arquivo, a linha e a chave.
- **RF-1.2** Toda chave tem default, documentado em [relay.example.toml](../reference/relay.example.toml). Chave desconhecida é **erro** (não aviso): num serviço de rede, um erro de digitação em `max_channels_per_host` que vira default silencioso é um limite que o operador acha que tem e não tem.
- **RF-1.3** Variáveis de ambiente sobrescrevem chaves, no formato `PORECATU_RELAY_<SEÇÃO>_<CHAVE>` (ex.: `PORECATU_RELAY_SERVER_BIND`). É o que torna o contêiner configurável sem montar arquivo.
- **RF-1.4** Não há recarga a quente no v0. Mudar a configuração é reiniciar o processo, o que derruba as sessões; host e cliente reconectam sozinhos.

### Tokens do operador

- **RF-1.5** A seção `[[tokens]]` lista os tokens aceitos, cada um com um `name` (para log e revogação) e um `value` (32 a 128 caracteres ASCII imprimíveis, [especificação §2](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). Nomes repetidos ou valor fora do formato: o processo não sobe.
- **RF-1.6** O valor do token **PODE** vir de arquivo (`value_file`) em vez de literal, para segredo montado pelo orquestrador (Docker secrets, systemd `LoadCredential`).
- **RF-1.7** `porecatu-relay token new [--name <nome>]` imprime um token novo (32 bytes aleatórios do sistema, base64url sem padding) e a linha TOML pronta para colar. Não escreve em arquivo nenhum.
- **RF-1.8** Revogar um token é tirá-lo do arquivo e reiniciar. Hosts registrados com ele caem e não voltam.
- **RF-1.9** O log de registro de host leva o **nome** do token usado, nunca o valor.

### Rede e TLS

- **RF-1.10** `[server] bind` define endereço e porta (default `0.0.0.0:443` com TLS, ver RF-1.11).
- **RF-1.11** Com `[tls] cert_file` e `key_file`, o relay termina TLS ele mesmo (`rustls`, TLS 1.3 por padrão, 1.2 opcional por `min_version`). O certificado é relido do disco **sem reiniciar** quando o arquivo muda (renovação do Let's Encrypt), sem derrubar conexões abertas.
- **RF-1.12** Sem TLS configurado, o relay só sobe se o `bind` é de loopback **ou** se `[server] behind_proxy = true`. Em qualquer outro caso, recusa subir e explica por quê ([ADR-0004](../adr/0004-tls-embutido-ou-atras-de-proxy.md)).
- **RF-1.13** Com `behind_proxy = true`, o IP do cliente nos logs vem do cabeçalho `X-Forwarded-For` **só** se a conexão vem de um endereço em `[server] trusted_proxies`.

### Limites

- **RF-1.14** São configuráveis, com default **igual ou acima** do piso da [especificação §6](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md): canais simultâneos por host, tamanho máximo de quadro, banda sustentada por canal (com rajada), hosts simultâneos no total, conexões de cliente por IP por minuto e tempo máximo para o host completar o registro.
- **RF-1.15** Estouro de limite **fecha** o canal ou a conexão com `limit-exceeded`/`4003` e registra o limite que estourou. Nunca descarta quadro em silêncio.
- **RF-1.16** Valor configurado abaixo do piso da especificação: o processo não sobe. Um relay que aceita menos do que a especificação garante quebra hosts e clientes conformes.

### Logs e observação

- **RF-1.17** Logs estruturados em `stderr`, em texto ou JSON (`[log] format`), com nível configurável.
- **RF-1.18** **Nenhum log, em nível nenhum, contém `payload`, token do operador, cabeçalho `Authorization` ou corpo de quadro.** O que pode aparecer: horário, IP (ver `[log] ip_addresses`), `host_id`, `channel`, nome do token, motivo de fechamento, duração e contagem de bytes. Há teste automatizado que roda uma sessão com marcador conhecido no `payload` e confere que ele não aparece na saída de log em nível `trace`.
- **RF-1.19** `[log] ip_addresses = "full" | "truncated" | "none"` (default `truncated`: `/24` em IPv4, `/48` em IPv6), para o operador que não quer guardar IP.
- **RF-1.20** `GET /healthz` responde `200 ok` quando o processo aceita conexões, sem autenticação e sem dado nenhum.
- **RF-1.21** Métricas no formato Prometheus em `GET /metrics`, **desligadas por padrão** (`[metrics] enabled`), num `bind` separado. Só contagens e histogramas agregados: hosts registrados, canais abertos, bytes encaminhados, fechamentos por motivo, latência de encaminhamento. **Nenhum rótulo com `host_id`, IP ou nome de token.**

### Ciclo de vida

- **RF-1.22** `SIGTERM`/`SIGINT` (e o equivalente no Windows): o relay para de aceitar conexões novas, fecha todos os canais com `closed { normal }` e as conexões de host com `1001`, e sai em até `[server] shutdown_grace_seconds` (default 10).
- **RF-1.23** O relay não guarda nada entre execuções; não há migração de dados ao atualizar.

### Empacotamento e instalação

- **RF-1.24** Binários para Linux (x86_64 e aarch64, `musl`, estáticos), macOS e Windows, publicados com `sha256` ao lado ([ADR-0005](../adr/0005-empacotamento-binario-e-docker.md)).
- **RF-1.25** Imagem de contêiner multi-arquitetura, a partir de imagem mínima, rodando como usuário não-root, sem shell.
- **RF-1.26** Unidade `systemd` de exemplo, com endurecimento (`DynamicUser`, `ProtectSystem=strict`, `NoNewPrivileges`, `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` só quando a porta é < 1024).
- **RF-1.27** `porecatu-relay --version` imprime a versão do relay e a do envelope que ele serve (`/v0/`).

### Atualização

- **RF-1.28** Atualizar é trocar o binário ou a imagem e reiniciar. Hosts e clientes reconectam.
- **RF-1.29** Uma versão do relay que passe a servir um envelope novo (`/v1/`) **continua servindo** o anterior por pelo menos uma versão menor do relay, e o CHANGELOG avisa quando o antigo sai ([versionamento](https://github.com/porecatu/protocol/blob/main/docs/especificacao/04-versionamento.md)).

### Presença

- **RF-1.30** O relay responde `GET /v0/presence/<host_id>` com `204` (host registrado) ou `404`, sem corpo; `400` para `host_id` mal formado; `429` acima do limite de consultas por origem (`[limits] presence_per_minute_per_ip`, default 120) ([01 §2](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md)). A consulta não abre canal, não toca a conexão do host e não vai para log além das contagens.

## Fora de escopo

- Recarga a quente da configuração (exceto o certificado, RF-1.11).
- Emissão automática de certificado (ACME embutido). O operador usa `certbot`, o proxy reverso ou o orquestrador.
- Autenticação de cliente no relay. Quem autentica o cliente é o host, pela sessão Noise.
- Painel web, API de administração, listagem de hosts registrados.

## Critérios de aceite

1. Um relay sobe com o [relay.example.toml](../reference/relay.example.toml) mais certificado e um token, e um host de teste se registra.
2. O teste de log do RF-1.18 passa em nível `trace`.
3. Cada limite do RF-1.14 tem teste que o estoura e confere o código de fechamento.
4. Configuração com valor abaixo do piso, chave desconhecida ou TLS ausente fora de loopback impede a subida, com mensagem que aponta a chave.
