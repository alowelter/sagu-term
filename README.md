<p align="center">
  <img src="assets/logo_mini.png" alt="SaguTerm" width="112">
</p>

<h1 align="center">SaguTerm</h1>

<p align="center">
  <strong>SSH, SFTP e terminal local num só lugar,<br>
  com todos os seus acessos protegidos num cofre criptografado.</strong>
</p>

<p align="center">
  <a href="https://apps.microsoft.com/detail/9N4S9R1BS9SM"><img alt="Microsoft Store" src="https://img.shields.io/badge/Microsoft%20Store-SaguTerm-6d28d9"></a>
  <img alt="Windows 10/11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078d4">
  <img alt="Feito em Rust" src="https://img.shields.io/badge/feito%20em-Rust-b7410e">
  <a href="LICENSE"><img alt="Licença MIT" src="https://img.shields.io/badge/licen%C3%A7a-MIT-green"></a>
</p>

<p align="center">
  <a href="https://apps.microsoft.com/detail/9N4S9R1BS9SM"><strong>⬇️ Baixar na Microsoft Store</strong></a>
  &nbsp;·&nbsp;
  <a href="#comecar">Comece em 1 minuto</a>
  &nbsp;·&nbsp;
  <a href="#atalhos">Atalhos</a>
</p>

<!--
  Dica: uma captura de tela aqui aumenta muito o interesse de quem visita.
  Salve em docs/screenshot.png (com paineis divididos, de preferencia sem IPs
  reais a mostra) e troque este comentario por:
  <p align="center"><img src="docs/screenshot.png" alt="SaguTerm em uso" width="900"></p>
-->

---

Se você administra servidores, conhece a rotina: um programa para SSH, outro para
transferir arquivos, senhas espalhadas em anotações e uma janela para cada servidor.
O **SaguTerm** junta tudo isso numa janela só, rápida e feita para ser usada pelo teclado.

## ✨ Por que experimentar

- **🛍️ Direto da Microsoft Store.** Instale com um clique. As versões novas chegam pela
  própria Store.
- **🔐 Suas senhas não ficam soltas.** Hosts, usuários, senhas e chaves privadas ficam num
  arquivo `.sagu` criptografado com **AES-256-GCM**, com a chave derivada da sua senha mestra
  por **Argon2id**. Leve o arquivo para outra máquina e abra com a mesma senha.
- **🪟 Vários servidores na mesma tela.** Divida a janela em quantos painéis quiser, lado a
  lado ou empilhados, no estilo do tmux, e misture sessões SSH, SFTP e terminais locais.
- **⌨️ Feito para o teclado.** Digite parte do nome, aperte Enter e você está conectado.
  Troque de painel com `Alt+setas`. O mouse é opcional.
- **📂 SFTP sem outro programa.** Navegue pelas pastas do servidor, arraste arquivos do
  Windows para enviá-los e baixe arquivos e pastas com `Ctrl+S`.
- **🛡️ Sabe com quem está falando.** Na primeira conexão, o SaguTerm mostra a impressão
  digital da chave do servidor e pede a sua confirmação. Se a chave mudar depois, ele avisa
  antes de enviar qualquer senha.
- **⚡ Leve e rápido.** Escrito em Rust do começo ao fim, incluindo o SSH (sem OpenSSL),
  com interface acelerada pela placa de vídeo.

## 🧰 Funcionalidades

### Terminal SSH
- Emulação xterm com 256 cores, que se ajusta sozinha ao tamanho do painel.
- **Selecionou, copiou:** o texto selecionado vai direto para a área de transferência.
  O **botão direito cola**.
- Autenticação por **senha** ou **chave privada** (formato OpenSSH ou PEM, com passphrase
  opcional).
- **Verificação da chave do servidor**, no SSH e no SFTP: na primeira conexão, uma janela
  mostra o tipo da chave e a impressão digital SHA256 para você conferir, com
  **Confiar e conectar** ou **Cancelar**. A chave aceita fica guardada no cofre, e as
  próximas conexões seguem direto enquanto ela for a mesma.
- Se o servidor apresentar outra chave, a conexão para num **alerta vermelho** com a
  impressão digital guardada e a nova, antes de enviar usuário, senha ou chave. Cancelar é
  o padrão: a chave nova só é aceita com um clique em **Aceitar a nova chave e conectar**.
- Keepalive automático, para a sessão não cair quando fica parada.
- Ao digitar `exit`, o painel fecha. Se a conexão cair, o painel fica aberto com o
  aviso, para você saber o que aconteceu.

### Terminal local
- **Prompt de Comando** e **WSL** (quando instalado) nos mesmos painéis dos servidores
  remotos, lado a lado.

### Painéis divididos
- Divida qualquer painel quantas vezes quiser: `Ctrl+B, H` (lado a lado) ou
  `Ctrl+B, V` (empilhado).
- Cada painel novo já abre o seletor de conexões com a busca pronta para digitar.
- Navegue com `Alt+setas`. O painel ativo fica destacado.

### Navegador SFTP
- Navegue pelas pastas com o teclado ou o mouse, ou digite o caminho direto.
- **Arraste arquivos** do Windows para enviar à pasta atual. Um arquivo de mesmo nome
  nessa pasta é substituído.
- **Baixe arquivos e pastas** para o computador: selecione os itens (`Ctrl+clique`,
  `Shift+clique` ou `Ctrl+A`), aperte `Ctrl+S` ou use o botão de download do cabeçalho
  e escolha a pasta de destino. Pastas vêm com todo o conteúdo, e o que não dá para
  baixar (como um link para outra pasta) aparece no resumo do fim.
- O andamento aparece no rodapé do painel, com **Cancelar**. Se algum item já existe no
  destino, você escolhe entre **Substituir**, **Pular existentes** ou **Cancelar**.
- Cada arquivo é baixado num temporário e só recebe o nome final quando termina, com a
  data de modificação do servidor. Nenhum arquivo pela metade fica com o nome final; se o
  app for fechado no meio do download, pode sobrar um temporário `.sagu-part` na pasta.
- Nomes que o Windows não aceita (como `CON`, `a:b` ou terminados em ponto) são ajustados,
  e nada é gravado fora da pasta que você escolheu.
- Renomeie, altere **permissões** (chmod), **proprietário/grupo** (chown) e exclua
  arquivos e pastas.

### Cofre e conexões
- Tela de conexões com **busca instantânea** pelo nome.
- Cadastre, edite e exclua hosts em qualquer seletor (`Ctrl+N` cria um novo).
- Ao editar um host, veja a impressão digital da chave do servidor guardada e, se
  precisar, use **Esquecer chave** para confirmá-la de novo na próxima conexão. Trocar o
  endereço ou a porta também apaga a chave guardada.
- Na tela de conexões, `Ctrl+L` **bloqueia o cofre** na hora.
- O cofre é salvo de forma atômica, então uma queda de energia no meio da gravação não
  corrompe o arquivo.

<a id="comecar"></a>

## 🚀 Comece em 1 minuto

1. **Instale o SaguTerm pela [Microsoft Store](https://apps.microsoft.com/detail/9N4S9R1BS9SM)**
   e abra-o pelo menu Iniciar.
2. **Crie o seu cofre:** escolha onde salvar o arquivo `.sagu` (de preferência numa pasta
   sua, como Documentos) e defina uma senha mestra.
3. **Cadastre um servidor** com `Ctrl+N` (nome, endereço, usuário e senha ou chave).
4. **Digite parte do nome e aperte Enter.**
5. **Na primeira conexão, confira a impressão digital** da chave do servidor e clique em
   **Confiar e conectar**. Pronto, você está conectado.

> 💡 A senha mestra não é guardada em lugar nenhum. Se você esquecê-la, não há como
> recuperar o cofre. Escolha uma senha forte e memorável.

<a id="atalhos"></a>

## ⌨️ Atalhos de teclado

A lista completa fica sempre à mão com **F1**, em qualquer tela (também em `Ctrl+B, A`).

| Onde | Atalho | Ação |
|---|---|---|
| Painéis | `Alt+setas` | Trocar de painel na direção da seta |
| Painéis | `Ctrl+B, H` / `Ctrl+B, V` | Dividir lado a lado / empilhado |
| Painéis | `Ctrl+B, O` | Ciclar entre os painéis |
| Painéis | `Ctrl+B, X` | Fechar o painel ativo |
| Painéis | `Ctrl+B, Ctrl+B` | Enviar `Ctrl+B` ao terminal (útil com tmux remoto) |
| Painéis | `Ctrl+B, F1` | Enviar `F1` ao terminal (ex.: ajuda do htop ou do mc) |
| Conexões | *digitar* | Filtrar pelo nome |
| Conexões | `Enter` / `Ctrl+Enter` | Conectar via SSH / abrir SFTP |
| Conexões | `Ctrl+N` | Cadastrar novo host |
| Conexões | `Ctrl+L` | Bloquear o cofre |
| SFTP | `Enter` / `Backspace` | Abrir pasta / voltar |
| SFTP | `Ctrl+clique` / `Shift+clique` / `Ctrl+A` | Marcar itens / selecionar um intervalo / selecionar tudo |
| SFTP | `Ctrl+S` | Baixar a seleção para o computador |
| SFTP | `F2` / `Delete` / `F5` | Renomear / excluir / atualizar |

## 🔐 Segurança

- **Cofre:** AES-256-GCM, com chave de 256 bits derivada da senha mestra por Argon2id e
  *salt* aleatório. O arquivo é autossuficiente e portátil. Não abra o cofre nas versões
  antigas 0.1.x: elas não conferem a chave do servidor e, ao salvar qualquer alteração,
  apagam as chaves guardadas. De volta a esta versão, cada servidor apareceria como novo.
- **Na memória:** a chave derivada é apagada logo após o uso.
- **Chave do servidor:** o SaguTerm confere a chave de cada servidor no estilo do PuTTY
  (*trust on first use*). Na primeira conexão, você confere a impressão digital SHA256 e
  decide se confia. A chave aceita fica guardada no cofre criptografado, e não no
  `known_hosts` do OpenSSH. Se o servidor apresentar outra chave depois, a conexão para
  num alerta antes de enviar usuário, senha ou chave, e só segue se você aceitar a chave
  nova com um clique. Cancelar (ou fechar o painel) encerra a conexão sem gravar nada.
- **Downloads:** os nomes que vêm do servidor são conferidos antes de gravar. Nomes com
  `..`, barras, caracteres proibidos no Windows, nomes reservados (`CON`, `NUL`...) ou
  fluxos alternativos do NTFS são ajustados ou ignorados, e nada sai da pasta de destino.
  Nada que já existe é substituído sem a sua confirmação.
- **Instalação e atualizações:** pela Microsoft Store, que entrega o pacote assinado. O app
  não tem atualizador próprio.
- **Privacidade:** sem conta, sem anúncios e sem telemetria. O app não envia nenhum dado ao
  desenvolvedor e não acessa a internet por conta própria: só se conecta aos servidores que
  você cadastra. Detalhes na [política de privacidade](PRIVACY.md).

## 🗺️ Próximos passos

- [x] Verificação da chave do servidor (guardada no cofre)
- [x] Download de arquivos e pastas pelo SFTP
- [x] Assinatura digital (o pacote é assinado pela Microsoft Store)
- [ ] Enviar pastas inteiras (hoje o envio por arrastar aceita só arquivos)
- [ ] Arrastar arquivos do navegador SFTP direto para o Explorador de Arquivos
- [ ] Renomear e excluir vários itens de uma vez no navegador SFTP

Tem uma ideia ou encontrou um problema? [Abra uma issue](https://github.com/alowelter/sagu-term/issues).
Toda sugestão ajuda.

## 🛠️ Compilando a partir do código

Requisitos: [Rust](https://rustup.rs) estável e o **Visual Studio Build Tools**
(componente "Desenvolvimento para desktop com C++", que inclui o Windows SDK).

```powershell
git clone https://github.com/alowelter/sagu-term.git
cd sagu-term
cargo build --release
# executável em target\release\SaguTerm.exe
```

Para gerar o pacote MSIX da Microsoft Store, rode `packaging/msix/build.ps1` depois do
`cargo build --release`. O passo a passo está em [packaging/msix/LOJA.md](packaging/msix/LOJA.md).

## 📄 Licença

Distribuído sob a licença [MIT](LICENSE). Use, modifique e compartilhe à vontade.

<p align="center"><sub>Feito em Rust por <a href="https://github.com/alowelter">Marcelo Welter</a>.</sub></p>
