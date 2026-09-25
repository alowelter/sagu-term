<p align="center">
  <img src="assets/logo_mini.png" alt="SaguTerm" width="112">
</p>

<h1 align="center">SaguTerm</h1>

<p align="center">
  <strong>SSH, SFTP e terminal local num só lugar: um único executável, sem instalação,<br>
  com todos os seus acessos protegidos num cofre criptografado.</strong>
</p>

<p align="center">
  <a href="https://github.com/alowelter/sagu-term/releases/latest"><img alt="Última versão" src="https://img.shields.io/github/v/release/alowelter/sagu-term?label=vers%C3%A3o&color=6d28d9"></a>
  <a href="https://github.com/alowelter/sagu-term/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/alowelter/sagu-term/total?label=downloads&color=6d28d9"></a>
  <img alt="Windows 10/11" src="https://img.shields.io/badge/Windows-10%20%7C%2011-0078d4">
  <img alt="Feito em Rust" src="https://img.shields.io/badge/feito%20em-Rust-b7410e">
  <a href="LICENSE"><img alt="Licença MIT" src="https://img.shields.io/badge/licen%C3%A7a-MIT-green"></a>
</p>

<p align="center">
  <a href="https://github.com/alowelter/sagu-term/releases/latest/download/SaguTerm.exe"><strong>⬇️ Baixar para Windows</strong></a>
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

- **🧳 Portátil de verdade.** Um único `.exe` com cerca de 15 MB. Não precisa instalar
  nada, nem o Visual C++ Redistributable. Roda até de um pendrive.
- **🔐 Suas senhas não ficam soltas.** Hosts, usuários, senhas e chaves privadas ficam num
  arquivo `.sagu` criptografado com **AES-256-GCM**, com a chave derivada da sua senha mestra
  por **Argon2id**. Leve o arquivo para outra máquina e abra com a mesma senha.
- **🪟 Vários servidores na mesma tela.** Divida a janela em quantos painéis quiser, lado a
  lado ou empilhados, no estilo do tmux, e misture sessões SSH, SFTP e terminais locais.
- **⌨️ Feito para o teclado.** Digite parte do nome, aperte Enter e você está conectado.
  Troque de painel com `Alt+setas`. O mouse é opcional.
- **📂 SFTP sem outro programa.** Navegue pelas pastas do servidor e arraste arquivos do
  Windows para enviá-los.
- **⚡ Leve e rápido.** Escrito em Rust do começo ao fim, incluindo o SSH (sem OpenSSL),
  com interface acelerada pela placa de vídeo.
- **🔄 Sempre atualizado.** O app avisa quando sai uma versão nova e se atualiza com um
  clique, conferindo a integridade do download.

## 🧰 Funcionalidades

### Terminal SSH
- Emulação xterm com 256 cores, que se ajusta sozinha ao tamanho do painel.
- **Selecionou, copiou:** o texto selecionado vai direto para a área de transferência.
  O **botão direito cola**.
- Autenticação por **senha** ou **chave privada** (formato OpenSSH ou PEM, com passphrase
  opcional).
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
- **Arraste arquivos** do Windows para enviar à pasta atual.
- Renomeie, altere **permissões** (chmod), **proprietário/grupo** (chown) e exclua
  arquivos e pastas.

### Cofre e conexões
- Tela de conexões com **busca instantânea** pelo nome.
- Cadastre, edite e exclua hosts em qualquer seletor (`Ctrl+N` cria um novo).
- Na tela de conexões, `Ctrl+L` **bloqueia o cofre** na hora.
- O cofre é salvo de forma atômica, então uma queda de energia no meio da gravação não
  corrompe o arquivo.

<a id="comecar"></a>

## 🚀 Comece em 1 minuto

1. **[Baixe o `SaguTerm.exe`](https://github.com/alowelter/sagu-term/releases/latest/download/SaguTerm.exe)**
   e coloque-o numa pasta sua (por exemplo, `Documentos\SaguTerm`).
2. **Abra o executável.** Na primeira vez, o Windows pode mostrar *"O Windows protegeu o
   computador"*, porque o executável ainda não tem assinatura digital paga. Clique em
   **Mais informações → Executar assim mesmo**.
3. **Crie o seu cofre:** escolha onde salvar o arquivo `.sagu` e defina uma senha mestra.
4. **Cadastre um servidor** com `Ctrl+N` (nome, endereço, usuário e senha ou chave).
5. **Digite parte do nome e aperte Enter.** Pronto, você está conectado.

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
| SFTP | `F2` / `Delete` / `F5` | Renomear / excluir / atualizar |

## 🔐 Segurança

- **Cofre:** AES-256-GCM, com chave de 256 bits derivada da senha mestra por Argon2id e
  *salt* aleatório. O arquivo é autossuficiente e portátil.
- **Na memória:** a chave derivada é apagada logo após o uso.
- **Atualizações:** só são instaladas depois que o SHA-256 do arquivo baixado confere com
  o publicado na Release. Os executáveis são compilados pelo GitHub Actions a partir do
  código público deste repositório, então qualquer pessoa pode conferir de onde vieram.
- **Transparência:** hoje o SaguTerm **não verifica a chave do servidor** (`known_hosts`)
  e aceita a chave apresentada na conexão. Essa verificação está nos planos. Até lá, evite
  conectar a partir de redes em que você não confia.

## 🗺️ Próximos passos

- [ ] Verificação da chave do servidor (`known_hosts`)
- [ ] Download de arquivos pelo SFTP
- [ ] Assinatura digital do executável

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

Para publicar uma versão, suba o `version` do `Cargo.toml` e envie uma tag `vX.Y.Z`.
O GitHub Actions compila, testa e publica a Release, e os apps em uso se atualizam
sozinhos.

## 📄 Licença

Distribuído sob a licença [MIT](LICENSE). Use, modifique e compartilhe à vontade.

<p align="center"><sub>Feito em Rust por <a href="https://github.com/alowelter">Marcelo Welter</a>.</sub></p>
