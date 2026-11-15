# Roadmap

Etapas de entrega do Porecatu Relay. Cada etapa tem **critério de saída explícito**; enquanto ele não for atendido, a etapa não terminou e a próxima não começa.

O relay anda junto com os outros dois lados do acesso remoto: o [Porecatu desktop](https://github.com/porecatu/terminal) (host) e o [Porecatu Remote](https://github.com/porecatu/remote) (cliente), sobre a [especificação do protocolo](https://github.com/porecatu/protocol). A primeira versão publicada sai **junto** com a primeira versão de fio `[0, 1]` do protocolo.

| Etapa | Status |
|---|---|
| R0 — Documentação | **em andamento** |
| R1 — Envelope, registro e roteamento | planejada |
| R2 — TLS, limites e configuração | planejada |
| R3 — Empacotamento e operação | planejada |
| R4 — Primeira versão | planejada |

---

## R0 — Documentação

Visão, requisitos de operação, arquitetura e as decisões de stack, cegueira, registro, TLS, empacotamento, licença e toolchain.

**Critério de saída:**

1. PRD-000, PRD-001, arquitetura e ADR-0001 a 0007 escritos e revisados pelo dono.
2. `python scripts/verify-docs.py` passa.
3. Nenhuma contradição com a [especificação, §1 a §7](https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md): onde houver dúvida, ela foi levada ao repositório do protocolo, não resolvida aqui.

## R1 — Envelope, registro e roteamento

Depende do crate `porecatu-protocol` com os tipos do envelope e o `host_id` (etapa correspondente do roadmap do protocolo).

- Esqueleto: `Cargo.toml` com lints, `rust-toolchain.toml`, CI nas três plataformas, `cargo deny`.
- `/v0/host` com token, desafio e registro; `/v0/client/{host_id}`; `/v0/presence/{host_id}`; `/healthz`.
- Registro em memória com geração; tabela de canais na host task; filas limitadas nos dois sentidos com a política da [arquitetura §5](arquitetura.md).
- Substituição de host, keepalive, encerramento.
- Tipo opaco do `payload` sem `Debug`, com o teste de compilação negativa.

**Critério de saída:**

1. Os testes de integração da [arquitetura §10](arquitetura.md) passam, em HTTP simples (loopback).
2. Os vetores de envelope do protocolo passam.
3. O teste de log cego passa em `trace`.
4. Um host e um cliente de teste trocam mil quadros em ordem.

## R2 — TLS, limites e configuração

- `relay.toml` completo, com validação, pisos da especificação e variáveis de ambiente ([PRD-001](prd/prd-001-operacao-do-relay.md) RF-1.1 a RF-1.9).
- TLS embutido com recarga de certificado; regra de subida sem TLS ([ADR-0004](adr/0004-tls-embutido-ou-atras-de-proxy.md)).
- Banda por canal, limites por IP, `X-Forwarded-For` de proxy confiável.
- Logs em texto e JSON, política de IP, métricas opcionais.

**Critério de saída:**

1. Cada limite do RF-1.14 tem teste que o estoura e confere o código.
2. Configuração inválida (chave desconhecida, abaixo do piso, TLS ausente fora de loopback) impede a subida com mensagem que aponta a chave.
3. Troca de certificado no disco é aplicada sem derrubar conexão aberta.

## R3 — Empacotamento e operação

- Binários `musl` e das outras plataformas, imagem de contêiner multi-arquitetura, unidade `systemd` ([ADR-0005](adr/0005-empacotamento-binario-e-docker.md)).
- Guia de operação: VPS com `certbot`, atrás de Caddy, atrás de nginx, Docker Compose, Raspberry Pi.
- `porecatu-relay token new`.
- Teste de carga contra as métricas do [PRD-000](prd/prd-000-visao-de-produto.md).

**Critério de saída:**

1. Alguém que não escreveu o relay o sobe num VPS seguindo só o README em menos de 15 minutos.
2. Memória por canal ocioso e latência acrescentada dentro do alvo, medidas e registradas.

## R4 — Primeira versão

**Critério de saída:**

1. O protocolo congelou a versão de fio `[0, 1]` ([versionamento](https://github.com/porecatu/protocol/blob/main/docs/especificacao/04-versionamento.md)).
2. Um Porecatu desktop e um Porecatu Remote de verdade conversam através do relay, num VPS, com TLS.
3. Release publicada com binários, `sha256`, imagem e CHANGELOG.

### Dívida de verificação prevista

Como no Porecatu, o que não for exercitado de verdade fica escrito aqui, como dívida, quando a etapa fechar — não como critério que impede a seguinte. Já se sabe que: macOS e Windows são só alvo de teste local; e operação atrás de proxies que não sejam Caddy e nginx não será verificada.

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
