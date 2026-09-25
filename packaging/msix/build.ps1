<#
.SYNOPSIS
    Gera o pacote MSIX do SaguTerm para a Microsoft Store.

.DESCRIPTION
    Monta target\msix\layout (SaguTerm.exe + AppxManifest.xml + Assets +
    resources.pri) e empacota em target\msix\SaguTerm-<versao>-x64.msix.

    - Versao: a do Cargo.toml (cargo metadata), com 4 partes (1.0.0 -> 1.0.0.0).
      O MSIX nao aceita a primeira parte 0 e a quarta e reservada para a Store.
    - Identidade: packaging\msix\identity.json, copiado de Partner Center >
      Product management > Product identity. Com -TestIdentity usa uma
      identidade de teste, para validar o pacote sem os dados reais.
    - Logos: gerados de assets\logo.png com System.Drawing.
    - Ferramentas: makepri.exe e makeappx.exe do Windows SDK 10 mais novo.

    O pacote sai sem assinatura: a Store assina no envio. Para testar
    localmente (Modo de Desenvolvedor), registre o layout com
    Add-AppxPackage -Register target\msix\layout\AppxManifest.xml.

    Roda no PowerShell 7 e no Windows PowerShell 5.1. Mantenha este arquivo
    so em ASCII: o 5.1 le .ps1 sem BOM como ANSI e estragaria acentos.

.PARAMETER Exe
    Executavel a empacotar. Padrao: target\release\SaguTerm.exe (rode
    "cargo build --release" antes). Entra no pacote como SaguTerm.exe.

.PARAMETER TestIdentity
    Usa a identidade de teste (Name SaguTerm.Dev, Publisher CN=SaguTermDev)
    em vez de identity.json. O pacote gerado NAO serve para a Store.

.EXAMPLE
    .\packaging\msix\build.ps1

.EXAMPLE
    .\packaging\msix\build.ps1 -TestIdentity

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\packaging\msix\build.ps1
    (Windows PowerShell 5.1, que por padrao nao roda scripts locais)
#>
[CmdletBinding()]
param(
    [string]$Exe,
    [switch]$TestIdentity
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$raiz = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$utf8 = New-Object System.Text.UTF8Encoding($false)

# Roda uma ferramenta do SDK e falha se o codigo de saida nao for 0. A saida e
# capturada e limpa: o makepri escreve UTF-16 quando nao fala com um console
# (sobram NULs no log) e o makeappx duplica as quebras de linha. Linhas que
# casam com $Omitir (regex) ficam fora do log.
function Invoke-Ferramenta([string]$Caminho, [string[]]$Argumentos, [string]$Omitir) {
    $nome = [IO.Path]::GetFileName($Caminho)
    Write-Host "> $nome $($Argumentos -join ' ')"
    # No 5.1, stderr redirecionado com 'Stop' viraria erro antes do codigo de saida
    $ErrorActionPreference = 'Continue'
    $saida = & $Caminho @Argumentos 2>&1
    $codigo = $LASTEXITCODE
    foreach ($linha in $saida) {
        $texto = ([string]$linha) -replace "`0", '' -replace "`r", ''
        if (-not $texto.Trim()) { continue }
        if ($Omitir -and $texto -match $Omitir) { continue }
        Write-Host "  $texto"
    }
    if ($codigo -ne 0) { throw "$nome falhou (codigo de saida $codigo)." }
}

# Publisher ID do Package Family Name: SHA-256 do Publisher em UTF-16LE, 8
# primeiros bytes em base32 Crockford (13 caracteres). E o sufixo que o
# Partner Center mostra no Package Family Name; serve para conferir a copia.
function Get-PublisherId([string]$Publisher) {
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $hash = $sha.ComputeHash([Text.Encoding]::Unicode.GetBytes($Publisher)) } finally { $sha.Dispose() }
    $bits = (-join ($hash[0..7] | ForEach-Object { [Convert]::ToString($_, 2).PadLeft(8, '0') })) + '0'
    $alfabeto = '0123456789abcdefghjkmnpqrstvwxyz'
    -join (0..12 | ForEach-Object { $alfabeto[[Convert]::ToInt32($bits.Substring($_ * 5, 5), 2)] })
}

# ---------------------------------------------------------------- executavel

if (-not $Exe) { $Exe = Join-Path $raiz 'target\release\SaguTerm.exe' }
$Exe = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Exe)
if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) {
    throw "Executavel nao encontrado: $Exe. Rode 'cargo build --release' antes (ou passe -Exe)."
}
# O manifesto declara x64: um exe de outra arquitetura reprova na certificacao
$pe = [IO.File]::ReadAllBytes($Exe)
$okPe = $pe.Length -ge 64 -and $pe[0] -eq 0x4D -and $pe[1] -eq 0x5A
if ($okPe) {
    $offPe = [BitConverter]::ToInt32($pe, 0x3C)
    $okPe = $offPe -gt 0 -and $offPe + 6 -le $pe.Length -and
        [BitConverter]::ToUInt32($pe, $offPe) -eq 0x4550 -and
        [BitConverter]::ToUInt16($pe, $offPe + 4) -eq 0x8664
}
if (-not $okPe) { throw "O executavel nao e um binario Windows x64: $Exe" }

# ------------------------------------------------------------------- versao

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw 'cargo nao encontrado no PATH (a versao do pacote vem do Cargo.toml).'
}
$json = & cargo metadata --no-deps --format-version 1 --manifest-path (Join-Path $raiz 'Cargo.toml') | Out-String
if ($LASTEXITCODE -ne 0) { throw "cargo metadata falhou (codigo de saida $LASTEXITCODE)." }
$pacote = @((ConvertFrom-Json $json).packages | Where-Object { $_.name -eq 'sagu-term' })
if ($pacote.Count -ne 1) { throw 'Pacote sagu-term nao encontrado na saida do cargo metadata.' }
$versaoCargo = [string]$pacote[0].version

# MSIX: 4 partes de 0 a 65535, primeira != 0, quarta reservada para a Store (0)
if ($versaoCargo -notmatch '^(\d+)\.(\d+)\.(\d+)$') {
    throw "Versao '$versaoCargo' do Cargo.toml nao serve para MSIX: use so X.Y.Z (sem -pre ou +build)."
}
$partes = @([int64]$Matches[1], [int64]$Matches[2], [int64]$Matches[3])
if (@($partes | Where-Object { $_ -gt 65535 }).Count -gt 0) {
    throw "Versao '$versaoCargo' do Cargo.toml nao serve para MSIX: cada parte vai de 0 a 65535."
}
if ($partes[0] -eq 0) {
    throw "Versao '$versaoCargo' do Cargo.toml nao serve para MSIX: a primeira parte nao pode ser 0 (use 1.0.0 ou maior)."
}
$versao = "$versaoCargo.0"

# --------------------------------------------------------------- identidade

$arqIdentidade = Join-Path $PSScriptRoot 'identity.json'
if ($TestIdentity) {
    $nomePacote = 'SaguTerm.Dev'
    $publisher = 'CN=SaguTermDev'
    $publisherDisplay = 'SaguTerm (teste)'
} else {
    if (-not (Test-Path -LiteralPath $arqIdentidade -PathType Leaf)) {
        throw "Arquivo nao encontrado: $arqIdentidade"
    }
    try {
        $id = [IO.File]::ReadAllText($arqIdentidade, $utf8) | ConvertFrom-Json
    } catch {
        throw "packaging\msix\identity.json nao e um JSON valido: $($_.Exception.Message)"
    }
    $valores = @{}
    $faltam = @()
    foreach ($campo in 'name', 'publisher', 'publisherDisplayName') {
        $prop = $id.PSObject.Properties[$campo]
        $valor = ''
        if ($prop -and $null -ne $prop.Value) { $valor = ([string]$prop.Value).Trim() }
        if (-not $valor) { $faltam += $campo }
        $valores[$campo] = $valor
    }
    if ($faltam.Count -gt 0) {
        throw ("packaging\msix\identity.json ainda nao foi preenchido (falta: $($faltam -join ', ')). " +
            'Copie os valores de Partner Center > SaguTerm > Product management > Product identity: ' +
            'name = Package/Identity/Name, publisher = Package/Identity/Publisher (comeca com CN=), ' +
            'publisherDisplayName = Package/Properties/PublisherDisplayName. ' +
            'Para testar o pacote sem esses dados, rode com -TestIdentity.')
    }
    $nomePacote = $valores['name']
    $publisher = $valores['publisher']
    $publisherDisplay = $valores['publisherDisplayName']
}
# Regras do esquema do manifesto (Identity/Name e Publisher)
if ($nomePacote -cnotmatch '^[A-Za-z0-9.\-]{3,50}$') {
    throw "identity.json: name '$nomePacote' invalido (3 a 50 caracteres: letras, numeros, ponto e hifen)."
}
if ($publisher -cnotmatch '^CN=') {
    throw "identity.json: publisher '$publisher' invalido: deve comecar com 'CN=' (copie identico do Partner Center)."
}
if ($publisher.Length -gt 8192) { throw 'identity.json: publisher com mais de 8192 caracteres.' }
if ($publisherDisplay.Length -gt 256) { throw 'identity.json: publisherDisplayName com mais de 256 caracteres.' }

# -------------------------------------------------------------------- SDK

$kits = $null
try { $kits = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -ErrorAction Stop).KitsRoot10 } catch { }
if (-not $kits) { $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10' }
# bin tem pastas por versao (10.0.26100.0...) e tambem x64/x86/arm64 soltas
$sdk = Get-ChildItem -LiteralPath (Join-Path $kits 'bin') -Directory -ErrorAction SilentlyContinue |
    Where-Object {
        $_.Name -match '^10\.\d+\.\d+\.\d+$' -and
        (Test-Path -LiteralPath (Join-Path $_.FullName 'x64\makeappx.exe')) -and
        (Test-Path -LiteralPath (Join-Path $_.FullName 'x64\makepri.exe'))
    } |
    Sort-Object { [version]$_.Name } -Descending | Select-Object -First 1
if (-not $sdk) {
    throw "Windows SDK 10 nao encontrado (makeappx.exe e makepri.exe em $kits\bin\<versao>\x64). Instale o Windows SDK."
}
$makeappx = Join-Path $sdk.FullName 'x64\makeappx.exe'
$makepri = Join-Path $sdk.FullName 'x64\makepri.exe'
Write-Host "Windows SDK: $(Join-Path $sdk.FullName 'x64')"

# ----------------------------------------------------------------- layout

$saida = Join-Path $raiz 'target\msix'
$layout = Join-Path $saida 'layout'
$assets = Join-Path $layout 'Assets'
$msix = Join-Path $saida "SaguTerm-$versao-x64.msix"

if (Test-Path -LiteralPath $layout) {
    try {
        Remove-Item -LiteralPath $layout -Recurse -Force
    } catch {
        throw ("Nao consegui apagar o layout anterior ($layout): $($_.Exception.Message) " +
            'Feche o SaguTerm e, se o layout estiver registrado, remova com Remove-AppxPackage.')
    }
}
New-Item -ItemType Directory -Force -Path $assets | Out-Null

Copy-Item -LiteralPath $Exe -Destination (Join-Path $layout 'SaguTerm.exe')

# Manifesto: troca os marcadores pelos valores ja escapados para XML
$modelo = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'AppxManifest.xml'), $utf8)
$marcadores = [ordered]@{
    '{{VERSION}}'                = $versao
    '{{IDENTITY_NAME}}'          = $nomePacote
    '{{PUBLISHER}}'              = $publisher
    '{{PUBLISHER_DISPLAY_NAME}}' = $publisherDisplay
}
foreach ($m in $marcadores.Keys) {
    if (-not $modelo.Contains($m)) { throw "Marcador $m ausente de packaging\msix\AppxManifest.xml." }
    $modelo = $modelo.Replace($m, [System.Security.SecurityElement]::Escape($marcadores[$m]))
}
if ($modelo -match '\{\{\w+\}\}') { throw "Marcador desconhecido em packaging\msix\AppxManifest.xml: $($Matches[0])" }
try { [xml]$modelo | Out-Null } catch { throw "Manifesto gerado nao e XML valido: $($_.Exception.Message)" }
$manifesto = Join-Path $layout 'AppxManifest.xml'
[IO.File]::WriteAllText($manifesto, $modelo, $utf8)

# ------------------------------------------------------------------ logos

# Tamanhos da Microsoft (base 100%). Wide310x150 e Square71x71 sao tiles do
# Windows 10 e ficam sem scale-400: o Windows usa a escala mais proxima e o
# Wide@400 encosta no limite de 200 KB por imagem do WACK.
$escalas = 100, 125, 150, 200, 400
$escalasTile = 100, 125, 150, 200
$alvos = 16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256
$icone = 0.94   # icones: o desenho ocupa quase todo o quadro
$tile = 0.66    # tiles: sobra espaco para o nome embaixo
$logos = @(
    @{ Nome = 'StoreLogo';         L = 50;  A = 50;  Escalas = $escalas;     Preench = $icone }
    @{ Nome = 'Square44x44Logo';   L = 44;  A = 44;  Escalas = $escalas;     Preench = $icone }
    @{ Nome = 'Square150x150Logo'; L = 150; A = 150; Escalas = $escalas;     Preench = $tile }
    @{ Nome = 'Wide310x150Logo';   L = 310; A = 150; Escalas = $escalasTile; Preench = $tile }
    @{ Nome = 'Square71x71Logo';   L = 71;  A = 71;  Escalas = $escalasTile; Preench = $tile }
)

# 62.5 -> 63 como na tabela da Microsoft ([Math]::Round padrao daria 62)
function Get-Px([double]$Base, [int]$Escala) {
    [int][Math]::Round($Base * $Escala / 100.0, [MidpointRounding]::AwayFromZero)
}

Add-Type -AssemblyName System.Drawing
$arqLogo = Join-Path $raiz 'assets\logo.png'
$origem = [System.Drawing.Image]::FromFile($arqLogo)
try {
    # So o hexagono (visivel em x 276..748, y 102..628, com 2 px de folga): o
    # texto "SAGU TERM" embaixo vira borrao nos icones de 16..32 px. O
    # recorte foi medido no logo 1024x1024; se o logo mudar, mude-o junto.
    if ($origem.Width -ne 1024 -or $origem.Height -ne 1024) {
        throw "assets\logo.png deveria ter 1024x1024 (tem $($origem.Width)x$($origem.Height)): revise o recorte do hexagono no build.ps1."
    }
    $recorte = New-Object System.Drawing.RectangleF(274, 100, 476, 530)

    # Desenha o hexagono centralizado num PNG transparente de L x A; Preench e
    # a fracao do lado menor que a altura do desenho ocupa.
    function Save-Logo([string]$Arquivo, [int]$L, [int]$A, [double]$Preench) {
        $bmp = New-Object System.Drawing.Bitmap($L, $A, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        try {
            $g = [System.Drawing.Graphics]::FromImage($bmp)
            try {
                $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
                $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
                $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
                # Sem SmoothingMode: no pwsh 7 ele engorda a borda em ~1 px e
                # o 5.1 o ignora; sem ele os dois geram os mesmos bytes.
                $g.Clear([System.Drawing.Color]::Transparent)
                $dh = [Math]::Min($L, $A) * $Preench
                $dw = $dh * $recorte.Width / $recorte.Height
                $dx = ($L - $dw) / 2
                $dy = ($A - $dh) / 2
                $pontos = [System.Drawing.PointF[]]@(
                    (New-Object System.Drawing.PointF($dx, $dy)),
                    (New-Object System.Drawing.PointF(($dx + $dw), $dy)),
                    (New-Object System.Drawing.PointF($dx, ($dy + $dh))))
                $attr = New-Object System.Drawing.Imaging.ImageAttributes
                try {
                    # evita o halo que o bicubico puxa de fora das bordas
                    $attr.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
                    $g.DrawImage($origem, $pontos, $recorte, [System.Drawing.GraphicsUnit]::Pixel, $attr)
                } finally { $attr.Dispose() }
            } finally { $g.Dispose() }
            $bmp.Save($Arquivo, [System.Drawing.Imaging.ImageFormat]::Png)
        } finally { $bmp.Dispose() }
    }

    foreach ($logo in $logos) {
        foreach ($escala in $logo.Escalas) {
            $arq = Join-Path $assets "$($logo.Nome).scale-$escala.png"
            Save-Logo $arq (Get-Px $logo.L $escala) (Get-Px $logo.A $escala) $logo.Preench
        }
    }
    # Icone da lista de apps/barra de tarefas em tamanhos exatos. As formas
    # unplated (sem placa de fundo, temas escuro e claro) sao o mesmo desenho.
    foreach ($t in $alvos) {
        $arq = Join-Path $assets "Square44x44Logo.targetsize-$t.png"
        Save-Logo $arq $t $t $icone
        Copy-Item -LiteralPath $arq -Destination (Join-Path $assets "Square44x44Logo.targetsize-$($t)_altform-unplated.png")
        Copy-Item -LiteralPath $arq -Destination (Join-Path $assets "Square44x44Logo.targetsize-$($t)_altform-lightunplated.png")
    }
} finally {
    $origem.Dispose()
}

$pngs = @(Get-ChildItem -LiteralPath $assets -Filter '*.png')
$grandes = @($pngs | Where-Object { $_.Length -ge 204800 })
if ($grandes.Count -gt 0) {
    throw "Imagens com 200 KB ou mais (o WACK reprova): $(($grandes | ForEach-Object { $_.Name }) -join ', ')"
}
Write-Host "Logos: $($pngs.Count) PNGs em $assets"

# ---------------------------------------------------------- resources.pri

# Os logos so existem com qualificador (scale-*/targetsize-*); sem o PRI o
# makeappx nao acha Assets\StoreLogo.png e afins e recusa o manifesto.
$qualificadas = @($pngs | Where-Object { $_.Name -match '\.(scale|targetsize)-' })
if ($qualificadas.Count -gt 0) {
    # Config fora do layout. Sem <packaging> (senao o makepri separa as escalas
    # em resources.scale-*.pri e o PRI principal fica so com scale-100) e sem
    # indexar o exe, o manifesto e PRIs antigos.
    $priconfig = Join-Path $saida 'priconfig.xml'
    $xmlPri = @'
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<resources targetOsVersion="10.0.0" majorVersion="1">
  <index root="\" startIndexAt="\">
    <default>
      <qualifier name="Language" value="pt-BR" />
      <qualifier name="Contrast" value="standard" />
      <qualifier name="Scale" value="100" />
      <qualifier name="HomeRegion" value="001" />
      <qualifier name="TargetSize" value="256" />
      <qualifier name="LayoutDirection" value="LTR" />
      <qualifier name="Theme" value="dark" />
      <qualifier name="AlternateForm" value="" />
      <qualifier name="DXFeatureLevel" value="DX9" />
      <qualifier name="Configuration" value="" />
      <qualifier name="DeviceFamily" value="Universal" />
      <qualifier name="Custom" value="" />
    </default>
    <indexer-config type="folder" foldernameAsQualifier="true" filenameAsQualifier="true" qualifierDelimiter=".">
      <exclude type="extension" value=".exe" doNotTraverse="true" doNotIndex="true" />
      <exclude type="name" value="AppxManifest.xml" doNotTraverse="true" doNotIndex="true" />
      <exclude type="extension" value=".pri" doNotTraverse="true" doNotIndex="true" />
    </indexer-config>
    <indexer-config type="resw" convertDotsToSlashes="true" initialPath="" />
    <indexer-config type="resjson" initialPath="" />
    <indexer-config type="PRI" />
  </index>
</resources>
'@
    [IO.File]::WriteAllText($priconfig, $xmlPri, $utf8)
    # O ResourceMap sai do Identity/Name do manifesto (/mn); /o evita a
    # pergunta de sobrescrever, que deixaria o script parado esperando
    # resposta. Sem /am nem /rm (WACK).
    Invoke-Ferramenta $makepri @('new', '/pr', $layout, '/cf', $priconfig, '/mn', $manifesto,
        '/of', (Join-Path $layout 'resources.pri'), '/o')
}

# ----------------------------------------------------------------- pacote

# Sem /nv: a validacao semantica do manifesto fica ligada. O log omite a
# linha "Processing ... as a payload file" de cada arquivo.
Invoke-Ferramenta $makeappx @('pack', '/d', $layout, '/p', $msix, '/o') 'as a payload file'

$publisherId = Get-PublisherId $publisher
$tamanho = '{0:N1}' -f ((Get-Item -LiteralPath $msix).Length / 1MB)
Write-Host ''
Write-Host "Pacote MSIX:         $msix ($tamanho MB)"
Write-Host "Layout:              $layout"
Write-Host "Versao:              $versao"
Write-Host "Identidade:          $nomePacote / $publisher / $publisherDisplay"
Write-Host "Package family name: $($nomePacote)_$publisherId"
Write-Host "Package full name:   $($nomePacote)_$($versao)_x64__$publisherId"
if ($TestIdentity) {
    Write-Host ''
    Write-Host 'Identidade de TESTE: este pacote nao serve para a Store. Para testar (Modo de Desenvolvedor):'
    Write-Host "  Add-AppxPackage -Register '$manifesto'"
} else {
    Write-Host '(o Package family name deve bater com o de Partner Center > Product identity)'
}
