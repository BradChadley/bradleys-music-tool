<#
  TagEditor.ps1 - the app's tag editor (PowerShell 7 and TagLib). The app carries this file and TagLibSharp.dll
  inside itself and writes them to its own data folder; nothing else is needed.

  The app copies the files it wants changed, then calls this once to change only the tags of those copies.
  For every file: the tags are rewritten on a second copy beside it, the audio is checked to be byte for byte
  the same and the change to be there, and only then the second copy takes the place of the first. Lyrics,
  ratings and DJ data are kept. It edits ONLY the files named in the plan.

  -Plan  JSON list of { File, Set: {title, artist, album, album_artist, genre, year, track, trackcount, disc, disccount,
         compilation}, Junk: [[field, value], ...], Cover: image path or null, CoverMode: 'all' | 'missing', Vbr: bool,
         CarryFrom: the file this one was converted from (its tags are carried over into the real fields, see
                    Copy-Tags), Skip: [field, ...] names (as ffprobe gives them) that must not be carried,
         Lyrics: words to save, LyricsMode: 'missing' (only when the file has none) | 'set' | 'clear',
         Unhide: bool (lyrics kept in a text tag no player reads go into the lyrics field),
         Exact: bool (refuse unless every other entry of the tag stays byte for byte the same),
         OnlyIf: the lyrics the file must hold now for its lyrics to be changed (undoing a lyrics repair),
         DropPersonal: bool (a download only: the rating, the play count and the DJ data another person's
                       player left in the file are taken out; never asked for the library's own files, whose
                       ratings are the owner's) }
  -Out   JSON list of { File, Ok, Why, Note, OldLyrics } is written here (OldLyrics: what the lyrics field
         held before, when this run changed it)
  -Functions  only load the functions (each parallel worker loads them this way)
#>
param([string]$Plan, [string]$Out, [string]$Ffmpeg, [switch]$Functions)
$ErrorActionPreference = 'Stop'

if (-not ('TagLib.File' -as [type])) { Add-Type -Path (Join-Path $PSScriptRoot 'TagLibSharp.dll') }
[TagLib.Id3v2.Tag]::DefaultVersion = 3
[TagLib.Id3v2.Tag]::ForceDefaultVersion = $false          # keep each file's own ID3 version
try { [Text.Encoding]::RegisterProvider([Text.CodePagesEncodingProvider]::Instance) } catch { }

# ---------------- file helpers ----------------
function Test-Locked([string]$path) { try { [IO.File]::Open($path, 'Open', 'ReadWrite', 'None').Close(); $false } catch { $true } }
# fingerprint of the audio data only (tags and pictures ignored) - must not change when only tags are edited
function Get-AudioHash([string]$path, [string]$forceFormat) {
    $a = @('-hide_banner', '-nostdin', '-v', 'error')
    if ($forceFormat) { $a += '-f', $forceFormat }
    $a += '-i', $path, '-map', '0:a', '-c', 'copy', '-f', 'md5', '-'
    (& $ffmpeg @a 2>$null) -join ''
}
function New-TempPath([string]$path) {
    $dir = Split-Path $path; $ext = [IO.Path]::GetExtension($path); $base = [IO.Path]::GetFileNameWithoutExtension($path)
    Join-Path $dir "$base.repairtmp$ext"     # keeps the real extension last so TagLib knows the format
}
# the checked copy takes the place of the file it was made from (itself a copy the app made: the app keeps the original)
function Complete-Swap($b, [string]$path, [string]$tmp, [string]$reason) { Move-Item -LiteralPath $tmp -Destination $path -Force }
function Short([string]$s, [int]$n = 70) { if ($s.Length -gt $n) { $s.Substring(0, $n) + '...' } else { $s } }

# ---------------- tags ----------------
# An MP3's ID3v2 tag must be fully readable before we let the tag editor rewrite it: if part of it can't be
# parsed (e.g. a picture whose stated size is wrong), saving would silently drop whatever is hidden behind it.
# Returns $null if the tag is clean, or a reason.
function Test-Id3Clean([string]$path) {
    if (-not $path.EndsWith('.mp3', [StringComparison]::OrdinalIgnoreCase)) { return $null }
    $fs = [IO.File]::OpenRead($path)
    try {
        $h = [byte[]]::new(10)
        if ($fs.Read($h, 0, 10) -lt 10 -or $h[0] -ne 0x49 -or $h[1] -ne 0x44 -or $h[2] -ne 0x33) { return $null }   # no ID3v2 tag at all: fine
        $ver = $h[3]; $flags = $h[5]
        if ($ver -lt 3 -or ($flags -band 0x80)) { return $null }    # v2.2 / unsynchronised tags: the editor handles these itself
        # (every byte is widened to [int] before shifting: in PowerShell, shifting a [byte] stays a byte and loses the high bits)
        $size = (([int]$h[6] -band 0x7f) -shl 21) -bor (([int]$h[7] -band 0x7f) -shl 14) -bor (([int]$h[8] -band 0x7f) -shl 7) -bor ([int]$h[9] -band 0x7f)
        $tag = [byte[]]::new($size); [void]$fs.Read($tag, 0, $size)
        $start = 0
        if ($flags -band 0x40) { $start = if ($ver -eq 4) { (([int]$tag[0] -band 0x7f) -shl 21) -bor (([int]$tag[1] -band 0x7f) -shl 14) -bor (([int]$tag[2] -band 0x7f) -shl 7) -bor ([int]$tag[3] -band 0x7f) } else { 4 + (([int]$tag[0] -shl 24) -bor ([int]$tag[1] -shl 16) -bor ([int]$tag[2] -shl 8) -bor [int]$tag[3]) } }
        # Some taggers (older iTunes especially) write frame sizes the v2.4 way inside v2.3 tags, and the tag editor
        # copes with that - so the tag counts as readable if EITHER way of reading sizes accounts for every byte.
        $worst = $null
        foreach ($syncsafe in @($ver -eq 4), @($ver -ne 4)) {
            $pos = $start; $why = $null
            while ($pos + 10 -le $size) {
                if ($tag[$pos] -eq 0) { break }        # start of padding
                $id = [Text.Encoding]::ASCII.GetString($tag, $pos, 4)
                if ($id -notmatch '^[A-Z0-9]{4}$') { $why = "its tag is damaged at byte $pos"; break }
                $fsz = if ($syncsafe[0]) { (([int]$tag[$pos + 4] -band 0x7f) -shl 21) -bor (([int]$tag[$pos + 5] -band 0x7f) -shl 14) -bor (([int]$tag[$pos + 6] -band 0x7f) -shl 7) -bor ([int]$tag[$pos + 7] -band 0x7f) }
                       else { ([int]$tag[$pos + 4] -shl 24) -bor ([int]$tag[$pos + 5] -shl 16) -bor ([int]$tag[$pos + 6] -shl 8) -bor [int]$tag[$pos + 7] }
                if ($fsz -lt 0 -or $pos + 10 + $fsz -gt $size) { $why = 'its tag is damaged (an entry runs past the end)'; break }
                $pos += 10 + $fsz
            }
            if (-not $why) {
                # everything after the last entry should be empty padding
                for ($i = $pos; $i -lt $size; $i++) { if ($tag[$i] -ne 0) { $why = "part of its tag ($([math]::Round(($size - $pos) / 1KB)) KB) can't be read"; break } }
            }
            if (-not $why) { return $null }        # this way of reading accounts for everything: clean
            if (-not $worst) { $worst = $why }
        }
        "$worst, so editing it could lose data - left alone"
    } finally { $fs.Dispose() }
}
# Some rippers leave a SECOND ID3v2 tag right after the first. The tag editor only edits the first one, but players
# and the scan read both, so junk in the second one would survive. Returns where it is and its text frames, or $null.
function Get-StackedId3([byte[]]$bytes) {
    if ($bytes.Length -lt 20 -or $bytes[0] -ne 0x49 -or $bytes[1] -ne 0x44 -or $bytes[2] -ne 0x33) { return $null }
    $syncsafe = { param($o) (([int]$bytes[$o] -band 0x7f) -shl 21) -bor (([int]$bytes[$o + 1] -band 0x7f) -shl 14) -bor (([int]$bytes[$o + 2] -band 0x7f) -shl 7) -bor ([int]$bytes[$o + 3] -band 0x7f) }
    $end1 = 10 + (& $syncsafe 6) + $(if ($bytes[5] -band 0x10) { 10 } else { 0 })
    if ($end1 + 10 -gt $bytes.Length -or $bytes[$end1] -ne 0x49 -or $bytes[$end1 + 1] -ne 0x44 -or $bytes[$end1 + 2] -ne 0x33) { return $null }
    $ver = $bytes[$end1 + 3]; $size2 = & $syncsafe ($end1 + 6)
    $frames = @(); $pos = $end1 + 10; $stop = $end1 + 10 + $size2
    while ($pos + 10 -le $stop) {
        $id = [Text.Encoding]::ASCII.GetString($bytes, $pos, 4); if ($id -notmatch '^[A-Z0-9]{4}$') { break }
        $fs = if ($ver -eq 4) { & $syncsafe ($pos + 4) } else { ([int]$bytes[$pos + 4] -shl 24) -bor ([int]$bytes[$pos + 5] -shl 16) -bor ([int]$bytes[$pos + 6] -shl 8) -bor [int]$bytes[$pos + 7] }
        if ($fs -le 0 -or $pos + 10 + $fs -gt $stop) { break }
        $enc = $bytes[$pos + 10]; $body = $pos + 11; $len = $fs - 1
        if ($id -eq 'COMM') { $body += 3; $len -= 3 }        # skip the language code
        $codec = switch ($enc) { 0 { [Text.Encoding]::Latin1 } 1 { [Text.Encoding]::Unicode } 2 { [Text.Encoding]::BigEndianUnicode } default { [Text.Encoding]::UTF8 } }
        $text = if ($len -gt 0 -and $id -match '^(T...|COMM)$') { ($codec.GetString($bytes, $body, $len) -replace '^﻿', '' -replace '\u0000+', "`0").Trim("`0") } else { '' }
        if ($id -eq 'COMM' -or $id -eq 'TXXX') { $text = ($text -split "`0")[-1] }   # drop the description
        $frames += [pscustomobject]@{ Id = $id; Text = $text.Trim() }
        $pos += 10 + $fs
    }
    [pscustomobject]@{ Start = $end1; Length = 10 + $size2; Frames = $frames }
}
# SHA-256 of a file's audio data only: the bytes between its tags, as the tag editor locates them.
# Editing tags must leave these bytes exactly as they were.
function Get-AudioBytesHash([string]$path) {
    $tf = [TagLib.File]::Create($path)
    try { $start = [long]$tf.InvariantStartPosition; $end = [long]$tf.InvariantEndPosition; $flac = $tf -is [TagLib.Flac.File] } finally { $tf.Dispose() }
    if ($end -le $start) { return $null }
    $fs = [IO.File]::OpenRead($path)
    try {
        $fs.Position = $start
        $sha = [Security.Cryptography.SHA256]::Create()
        if ($flac) {
            # A FLAC file keeps its tags and pictures in blocks between its "fLaC" mark and the sound, and the
            # tag editor counts those blocks as part of the file's middle: the sound starts after the last
            # block. The first block says what the sound is (its length, and a checksum of it): it must stay
            # as it is too, so it is part of the fingerprint.
            $h = [byte[]]::new(4)
            if ($fs.Read($h, 0, 4) -lt 4 -or [Text.Encoding]::ASCII.GetString($h) -ne 'fLaC') { return $null }
            do {
                if ($fs.Read($h, 0, 4) -lt 4) { return $null }
                $len = ([int]$h[1] -shl 16) -bor ([int]$h[2] -shl 8) -bor [int]$h[3]
                if (([int]$h[0] -band 0x7f) -eq 0) {
                    $info = [byte[]]::new($len); if ($fs.Read($info, 0, $len) -lt $len) { return $null }
                    [void]$sha.TransformBlock($info, 0, $len, $null, 0)
                } else { $fs.Position += $len }
            } until ([int]$h[0] -band 0x80)
            $start = $fs.Position
            if ($end -le $start) { return $null }
        }
        $buf = [byte[]]::new(1MB); $left = $end - $start
        while ($left -gt 0) {
            $n = $fs.Read($buf, 0, [int][math]::Min($buf.Length, $left)); if ($n -le 0) { break }
            [void]$sha.TransformBlock($buf, 0, $n, $null, 0); $left -= $n
        }
        [void]$sha.TransformFinalBlock($buf, 0, 0)
        "$($end - $start):" + [Convert]::ToHexString($sha.Hash)
    } finally { $fs.Dispose() }
}
# reads (key, value) pairs the way ffprobe names them, for matching the scan's exact junk values
function Get-JunkPairs($tf) {
    $pairs = @()
    $id3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
    if ($id3) {
        foreach ($fr in $id3.GetFrames()) {
            $id = $fr.FrameId.ToString()
            switch -Regex ($id) {
                '^COMM$' { if ($fr.Description -notmatch '^iTun') { $pairs += [pscustomobject]@{ Key = 'comment'; Value = $fr.Text; Frame = $fr; Tag = $id3 } } }
                '^TENC$' { $pairs += [pscustomobject]@{ Key = 'encoded_by'; Value = ($fr.Text -join '/'); Frame = $fr; Tag = $id3 } }
                '^TCOP$' { $pairs += [pscustomobject]@{ Key = 'copyright'; Value = ($fr.Text -join '/'); Frame = $fr; Tag = $id3 } }
                '^TPUB$' { $pairs += [pscustomobject]@{ Key = 'publisher'; Value = ($fr.Text -join '/'); Frame = $fr; Tag = $id3 } }
                '^TXXX$' { $pairs += [pscustomobject]@{ Key = $fr.Description.ToLowerInvariant(); Value = ($fr.Text -join '/'); Frame = $fr; Tag = $id3 } }
            }
        }
    }
    $x = $tf.GetTag([TagLib.TagTypes]::Xiph, $false)
    # (ffprobe calls a FLAC's DESCRIPTION its comment, so that is the name the scan knows it by)
    if ($x) { foreach ($k in $x) { foreach ($v in $x.GetField($k)) { $pairs += [pscustomobject]@{ Key = ($k.ToLowerInvariant() -replace '-', '_' -replace '^description$', 'comment'); Value = $v; Field = $k; Tag = $x } } } }
    # the old ID3v1 tag at the end of MP3s: players fall back to it, and junk often hides there
    $v1 = $tf.GetTag([TagLib.TagTypes]::Id3v1, $false)
    if ($v1 -and $v1.Comment) { $pairs += [pscustomobject]@{ Key = 'comment'; Value = $v1.Comment; Apple = 'Comment'; Tag = $v1 } }
    $ap = $tf.GetTag([TagLib.TagTypes]::Apple, $false)
    if ($ap) {
        if ($ap.Comment) { $pairs += [pscustomobject]@{ Key = 'comment'; Value = $ap.Comment; Apple = 'Comment'; Tag = $ap } }
        if ($ap.Copyright) { $pairs += [pscustomobject]@{ Key = 'copyright'; Value = $ap.Copyright; Apple = 'Copyright'; Tag = $ap } }
    }
    $pairs
}
function Same([string]$a, [string]$b) { ($a -replace '\s+', ' ').Trim() -eq ($b -replace '\s+', ' ').Trim() }
# a tag value as one line of text ('' = there is none): lists joined, a number 0 is "none"
function Text($v) {
    if ($null -eq $v) { return '' }
    if ($v -is [string]) { return $v.Trim() }
    if ($v -is [System.Collections.IEnumerable]) { return (@($v | ForEach-Object { "$_".Trim() } | Where-Object { $_ }) -join '/') }
    if ($v -is [double]) { if ([double]::IsNaN($v)) { return '' } else { return $v.ToString([Globalization.CultureInfo]::InvariantCulture) } }
    if ($v -is [ValueType] -and "$v" -eq '0') { return '' }
    "$v".Trim()
}

# ---------------- lyrics ----------------
# ffmpeg cannot write a lyrics frame: when it converts a file it leaves the words in a generic text tag
# named after the field, which no player reads. These are the names it uses.
function Test-HiddenLyricName([string]$d) { $d -match '^(USLT|UNSYNCEDLYRICS|UNSYNCED LYRICS|LYRICS|lyrics-.*)$' }
# the words in the field players read ('' = none)
function Get-RealLyrics($tf) {
    $l = "$($tf.Tag.Lyrics)"
    if (-not $l.Trim()) {
        $x = $tf.GetTag([TagLib.TagTypes]::Xiph, $false)
        if ($x) { foreach ($k in 'UNSYNCEDLYRICS', 'UNSYNCED LYRICS') { $v = $x.GetFirstField($k); if ("$v".Trim()) { $l = "$v"; break } } }
    }
    if ($l.Trim()) { $l } else { '' }
}
# the words in a text tag no player reads ('' = none); the longest, if there is more than one
function Get-HiddenLyrics($tf) {
    $best = ''
    $id3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
    if ($id3) {
        foreach ($fr in $id3.GetFrames()) {
            if ($fr -is [TagLib.Id3v2.UserTextInformationFrame] -and (Test-HiddenLyricName "$($fr.Description)")) {
                $v = ($fr.Text -join "`n"); if ($v.Trim().Length -gt $best.Trim().Length) { $best = $v }
            }
        }
    }
    $best
}
function Clear-Lyrics($tf) {
    $tf.Tag.Lyrics = $null
    $id3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
    if ($id3) { foreach ($id in 'USLT', 'SYLT') { $id3.RemoveFrames([TagLib.ByteVector]::FromString($id, [TagLib.StringType]::Latin1)) } }
    $x = $tf.GetTag([TagLib.TagTypes]::Xiph, $false)
    if ($x) { foreach ($k in 'UNSYNCEDLYRICS', 'UNSYNCED LYRICS') { $x.RemoveField($k) } }
}

# ---------------- carrying a converted file's tags over ----------------
# ffmpeg carries the plain fields across a conversion (title, artist, album, date, genre, track, disc...) and
# leaves the rest as generic text tags that only have a field's name, or drops them. Copy-Tags reads the file
# the conversion was made from and writes each of those into its real place in the new file, only where the
# new file has nothing there yet (what the import set on purpose stays). It does not depend on what the new
# file's format is: the plain fields go through the tag library's own names; an MP3 gets its frames besides.
function Get-Carriers {
    @(
        # (read from the field, else from a tag that hides them; "the new file has them" means in the field)
        @{ N = 'lyrics'; Skip = @('lyrics'); Read = { param($f) $l = Get-RealLyrics $f; if ($l) { $l } else { Get-HiddenLyrics $f } }; Has = { param($f) Get-RealLyrics $f }; Write = { param($f, $v) $f.Tag.Lyrics = $v } }
        @{ N = 'comment'; Skip = @('comment', 'comments', 'description'); Read = { param($f) $c = $f.Tag.Comment; if (-not "$c".Trim()) { $x = $f.GetTag([TagLib.TagTypes]::Xiph, $false); if ($x) { foreach ($k in 'COMMENT', 'DESCRIPTION') { $c = $x.GetFirstField($k); if ("$c".Trim()) { break } } } }; $c }; Has = { param($f) $f.Tag.Comment }; Write = { param($f, $v) $f.Tag.Comment = $v } }
        @{ N = 'bpm'; Skip = @('bpm', 'tbpm'); Read = { param($f) $f.Tag.BeatsPerMinute }; Write = { param($f, $v) $f.Tag.BeatsPerMinute = [uint32]$v } }
        @{ N = 'grouping'; Skip = @('grouping'); Read = { param($f) $f.Tag.Grouping }; Write = { param($f, $v) $f.Tag.Grouping = $v } }
        @{ N = 'conductor'; Skip = @('conductor'); Read = { param($f) $f.Tag.Conductor }; Write = { param($f, $v) $f.Tag.Conductor = $v } }
        @{ N = 'isrc'; Skip = @('isrc', 'tsrc'); Read = { param($f) $f.Tag.ISRC }; Write = { param($f, $v) $f.Tag.ISRC = $v } }
        @{ N = 'publisher'; Skip = @('publisher', 'label', 'organization'); Read = { param($f) $p = $f.Tag.Publisher; if (-not "$p".Trim()) { $x = $f.GetTag([TagLib.TagTypes]::Xiph, $false); if ($x) { $p = $x.GetFirstField('LABEL') } }; $p }; Write = { param($f, $v) $f.Tag.Publisher = $v } }
        @{ N = 'key'; Skip = @('key', 'initialkey'); Read = { param($f) $f.Tag.InitialKey }; Write = { param($f, $v) $f.Tag.InitialKey = $v } }
        @{ N = 'remixer'; Skip = @('remixer'); Read = { param($f) $f.Tag.RemixedBy }; Write = { param($f, $v) $f.Tag.RemixedBy = $v } }
        @{ N = 'subtitle'; Skip = @('subtitle'); Read = { param($f) $f.Tag.Subtitle }; Write = { param($f, $v) $f.Tag.Subtitle = $v } }
        @{ N = 'title sort'; Skip = @('title'); Read = { param($f) $f.Tag.TitleSort }; Write = { param($f, $v) $f.Tag.TitleSort = $v } }
        @{ N = 'album sort'; Skip = @('album'); Read = { param($f) $f.Tag.AlbumSort }; Write = { param($f, $v) $f.Tag.AlbumSort = $v } }
        @{ N = 'artist sort'; Skip = @('artist'); Read = { param($f) , @($f.Tag.PerformersSort) }; Write = { param($f, $v) $f.Tag.PerformersSort = [string[]]@($v) } }
        @{ N = 'album artist sort'; Skip = @('album_artist'); Read = { param($f) , @($f.Tag.AlbumArtistsSort) }; Write = { param($f, $v) $f.Tag.AlbumArtistsSort = [string[]]@($v) } }
        @{ N = 'composer sort'; Skip = @('composer'); Read = { param($f) , @($f.Tag.ComposersSort) }; Write = { param($f, $v) $f.Tag.ComposersSort = [string[]]@($v) } }
        @{ N = 'track total'; Skip = @('track'); Read = { param($f) $f.Tag.TrackCount }; Write = { param($f, $v) $f.Tag.TrackCount = [uint32]$v } }
        @{ N = 'disc total'; Skip = @('disc'); Read = { param($f) $f.Tag.DiscCount }; Write = { param($f, $v) $f.Tag.DiscCount = [uint32]$v } }
        @{ N = 'musicbrainz artist'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzArtistId }; Write = { param($f, $v) $f.Tag.MusicBrainzArtistId = $v } }
        @{ N = 'musicbrainz album'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseId }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseId = $v } }
        @{ N = 'musicbrainz album artist'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseArtistId }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseArtistId = $v } }
        @{ N = 'musicbrainz track'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzTrackId }; Write = { param($f, $v) $f.Tag.MusicBrainzTrackId = $v } }
        @{ N = 'musicbrainz release group'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseGroupId }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseGroupId = $v } }
        @{ N = 'musicbrainz disc'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzDiscId }; Write = { param($f, $v) $f.Tag.MusicBrainzDiscId = $v } }
        @{ N = 'musicbrainz status'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseStatus }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseStatus = $v } }
        @{ N = 'musicbrainz type'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseType }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseType = $v } }
        @{ N = 'musicbrainz country'; Skip = @(); Read = { param($f) $f.Tag.MusicBrainzReleaseCountry }; Write = { param($f, $v) $f.Tag.MusicBrainzReleaseCountry = $v } }
    )
}
# The plain fields, for a file made into a FLAC. ffmpeg carries them when it can read them and when the
# file it reads names them the way it expects; a tag at the end of an AIFF file it does not read at all, and
# the year of a WavPack or Monkey's Audio file it writes under a name FLAC players do not look at. So for
# a FLAC these are carried like the rest: only where the new file has nothing there.
function Get-PlainCarriers {
    @(
        @{ N = 'title'; Skip = @('title'); Read = { param($f) $f.Tag.Title }; Write = { param($f, $v) $f.Tag.Title = $v } }
        @{ N = 'artist'; Skip = @('artist'); Read = { param($f) , @($f.Tag.Performers) }; Write = { param($f, $v) $f.Tag.Performers = [string[]]@($v) } }
        @{ N = 'album'; Skip = @('album'); Read = { param($f) $f.Tag.Album }; Write = { param($f, $v) $f.Tag.Album = $v } }
        @{ N = 'album artist'; Skip = @('album_artist'); Read = { param($f) , @($f.Tag.AlbumArtists) }; Write = { param($f, $v) $f.Tag.AlbumArtists = [string[]]@($v) } }
        @{ N = 'genre'; Skip = @('genre'); Read = { param($f) , @($f.Tag.Genres) }; Write = { param($f, $v) $f.Tag.Genres = [string[]]@($v) } }
        @{ N = 'year'; Skip = @('date', 'year'); Read = { param($f) $f.Tag.Year }; Write = { param($f, $v) $f.Tag.Year = [uint32]$v } }
        @{ N = 'track'; Skip = @('track'); Read = { param($f) $f.Tag.Track }; Write = { param($f, $v) $f.Tag.Track = [uint32]$v } }
        @{ N = 'disc'; Skip = @('disc'); Read = { param($f) $f.Tag.Disc }; Write = { param($f, $v) $f.Tag.Disc = [uint32]$v } }
        @{ N = 'composer'; Skip = @('composer'); Read = { param($f) , @($f.Tag.Composers) }; Write = { param($f, $v) $f.Tag.Composers = [string[]]@($v) } }
    )
}
function Get-OriginalYear($id3) {
    foreach ($id in 'TDOR', 'TORY') { $t = "$($id3.GetTextAsString((Id3Name $id)))"; if ($t -match '(\d{4})') { return $Matches[1] } }
    ''
}
function Id3Name([string]$id) { , [TagLib.ByteVector]::FromString($id, [TagLib.StringType]::Latin1) }
# the frames of an MP3 that ffmpeg drops or turns into generic text tags when it rewrites the file, and which
# field (as ffprobe names it) each one stands for
$CarriedFrames = @{
    USLT = 'lyrics'; SYLT = 'lyrics'; COMM = 'comment'; POPM = ''; PCNT = ''; GEOB = ''; UFID = ''; TIT1 = 'grouping'; TIT3 = 'subtitle'
    TBPM = 'bpm'; TKEY = 'key'; TSRC = 'isrc'; TPUB = 'publisher'; TPE3 = 'conductor'; TPE4 = 'remixer'; TEXT = 'lyricist'; TOLY = ''; TOPE = ''
    TOAL = ''; TORY = ''; TDOR = ''; TSOP = 'artist'; TSOA = 'album'; TSOT = 'title'; TSO2 = 'album_artist'; TSOC = 'composer'; TCMP = 'compilation'
    TMOO = ''; TLAN = ''; TMED = ''; TDRL = ''; TIPL = ''; TMCL = ''; IPLS = ''; RVA2 = ''; RVAD = ''; MCDI = ''; CHAP = ''; CTOC = ''; ETCO = ''
}
# What another person's player left in a download: their rating, how often they played it, and their DJ
# software's cue points and loops. These say something about that person, not about the music.
# (The patterns are written inside the functions on purpose: a pattern held in a variable that turned out
# empty would match every entry, and everything would be taken out.)
function Test-PersonalName([string]$name) { $name -match '^(rating.*|fmps_(rating|playcount).*|play[ _]?count(er)?)$' }
function Test-PersonalFrame($fr) {
    if ($fr.FrameId.ToString() -in 'POPM', 'PCNT', 'GEOB') { return $true }
    if ($fr -is [TagLib.Id3v2.UserTextInformationFrame]) { return Test-PersonalName "$($fr.Description)" }
    if ($fr -is [TagLib.Id3v2.PrivateFrame]) { return "$($fr.Owner)" -match 'traktor|serato|rekordbox' }
    $false
}
# Takes them out of the open file (or, with $remove off, only counts them). Returns how many there were.
function Remove-Personal($tf, [bool]$remove = $true) {
    $n = 0
    $id3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
    if ($id3) { foreach ($fr in @($id3.GetFrames())) { if (Test-PersonalFrame $fr) { if ($remove) { $id3.RemoveFrame($fr) }; $n++ } } }
    $x = $tf.GetTag([TagLib.TagTypes]::Xiph, $false)
    if ($x) { foreach ($k in @($x)) { if (Test-PersonalName "$k") { if ($remove) { $x.RemoveField($k) }; $n++ } } }
    $n
}
# Returns what was carried, as name -> the value to find there afterwards. $dropPersonal: see Remove-Personal
# (what it would take out is not carried in the first place).
function Copy-Tags($tf, [string]$source, [string[]]$skip, [bool]$dropPersonal = $false) {
    $want = @{}
    $skip = @($skip | ForEach-Object { "$_".ToLowerInvariant() })
    $sf = [TagLib.File]::Create($source)
    try {
        $tid3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
        $sid3 = $sf.GetTag([TagLib.TagTypes]::Id3v2, $false)
        # an MP3 made from an MP3: the frames themselves, as they are (a rating, play count, DJ data, timed
        # lyrics, lyrics in two languages...), where the new file has none of that kind
        if ($tid3 -and $sid3) {
            $have = @{}; foreach ($fr in $tid3.GetFrames()) { $have[$fr.FrameId.ToString()] = 1 }
            $haveText = @{}; $haveOwner = @{}
            foreach ($fr in $tid3.GetFrames()) {
                if ($fr -is [TagLib.Id3v2.UserTextInformationFrame]) { $haveText["$($fr.Description)".ToLowerInvariant()] = 1 }
                if ($fr -is [TagLib.Id3v2.PrivateFrame]) { $haveOwner["$($fr.Owner)"] = 1 }
            }
            foreach ($fr in @($sid3.GetFrames())) {
                if ($dropPersonal -and (Test-PersonalFrame $fr)) { continue }
                $id = $fr.FrameId.ToString()
                $add = $false
                if ($fr -is [TagLib.Id3v2.UserTextInformationFrame]) {
                    $d = "$($fr.Description)".ToLowerInvariant()
                    $add = -not $haveText[$d] -and $skip -notcontains $d -and "$($fr.Text -join '')".Trim()
                } elseif ($fr -is [TagLib.Id3v2.PrivateFrame]) {
                    $add = -not $haveOwner["$($fr.Owner)"]
                } elseif ($CarriedFrames.ContainsKey($id) -and -not $have[$id]) {
                    $field = $CarriedFrames[$id]
                    $add = -not $field -or $skip -notcontains $field
                    # a comment the import took out as junk stays out; the sound-check notes players keep there come along
                    if ($id -eq 'COMM' -and -not $add) { $add = "$($fr.Description)" -match '^iTun' }
                }
                if ($add) { $tid3.AddFrame($fr); $want["frames $id"] = 1 + [int]$want["frames $id"] }
            }
        }
        $carriers = @(Get-Carriers); if ($tf -is [TagLib.Flac.File]) { $carriers = @(Get-PlainCarriers) + $carriers }
        foreach ($c in $carriers) {
            if ($c.Skip | Where-Object { $skip -contains $_ }) { continue }
            $v = & $c.Read $sf
            if (-not (Text $v)) { continue }
            $has = if ($c.Has) { $c.Has } else { $c.Read }
            if (Text (& $has $tf)) { continue }          # the new file already has it
            & $c.Write $tf $v
            $want["carry $($c.N)"] = Text $v
        }
        # what only an MP3's tag has a place for
        $tid3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
        if ($tid3) {
            $x = $sf.GetTag([TagLib.TagTypes]::Xiph, $false); $ap = $sf.GetTag([TagLib.TagTypes]::Apple, $false)
            $comp = ($x -and "$($x.GetFirstField('COMPILATION'))".Trim() -eq '1') -or ($ap -and $ap.IsCompilation) -or ($sid3 -and $sid3.IsCompilation)
            if ($comp -and -not $tid3.IsCompilation -and $skip -notcontains 'compilation') { $tid3.IsCompilation = $true; $want['carry compilation'] = '1' }
            if ($x) {
                $orig = "$($x.GetFirstField('ORIGINALDATE'))"; if (-not $orig.Trim()) { $orig = "$($x.GetFirstField('ORIGINALYEAR'))" }
                if ($orig -match '(\d{4})') {
                    # (the tag library keeps the original year under one name and writes it as each version of the tag wants)
                    if (-not (Get-OriginalYear $tid3)) { $tid3.SetTextFrame((Id3Name 'TDOR'), [string[]]@($Matches[1])); $want['original year'] = $Matches[1] }
                }
                $by = "$($x.GetFirstField('LYRICIST'))".Trim()
                if ($by -and -not $tid3.GetTextAsString((Id3Name 'TEXT'))) { $tid3.SetTextFrame((Id3Name 'TEXT'), [string[]]@($by)); $want['text TEXT'] = $by }
            }
        }
    } finally { $sf.Dispose() }
    $want
}
# After a carry: the generic text tags ffmpeg wrote are taken out, each only when the real field now holds
# the same value (so nothing is lost if a field could not be written). $lyricsOnly: only the ones that hide lyrics.
function Remove-StandIns($tf, [bool]$lyricsOnly) {
    # A FLAC made from a file with an MP3-style tag: what ffmpeg has no name for it leaves under that tag's
    # own four-letter names, and lyrics under "lyrics-" and their language. One made from a WavPack or
    # Monkey's Audio file keeps that tag's "Year" and "Album Artist". FLAC players read none of these.
    # (Only these names: every other entry of a FLAC's tag is the real field itself.)
    $x = $null
    if ($tf -is [TagLib.Flac.File]) { $x = $tf.GetTag([TagLib.TagTypes]::Xiph, $false) }   # (set on its own line: as the value of an "if" the tag would arrive as a list of its names)
    if ($x) {
        $n = 0
        foreach ($k in @($x)) {
            if ($k -match '^(USLT|LYRICS-.+)$') {
                if (Same (Get-RealLyrics $tf) ($x.GetField($k) -join "`n")) { $x.RemoveField($k); $n++ }
                continue
            }
            if ($lyricsOnly) { continue }
            $real = switch -Regex ($k) {
                '^TBPM$' { $tf.Tag.BeatsPerMinute; break }
                '^TIT1$' { $tf.Tag.Grouping; break }
                '^TIT3$' { $tf.Tag.Subtitle; break }
                '^TSRC$' { $tf.Tag.ISRC; break }
                '^TPUB$' { $tf.Tag.Publisher; break }
                '^TKEY$' { $tf.Tag.InitialKey; break }
                '^TPE3$' { $tf.Tag.Conductor; break }
                '^TPE4$' { $tf.Tag.RemixedBy; break }
                '^YEAR$' { $d = "$($x.GetFirstField('DATE'))".Trim(); if ($d.Length -ge 4) { $d.Substring(0, 4) } else { '' }; break }
                '^ALBUM ARTIST$' { $x.GetFirstField('ALBUMARTIST'); break }
                default { $null }
            }
            if ((Text $real) -and (Same (Text $real) ($x.GetField($k) -join '/'))) { $x.RemoveField($k); $n++ }
        }
        return $n
    }
    $id3 = $tf.GetTag([TagLib.TagTypes]::Id3v2, $false)
    if (-not $id3) { return 0 }
    $n = 0
    $text = { param($id) $id3.GetTextAsString((Id3Name $id)) }
    foreach ($fr in @($id3.GetFrames() | Where-Object { $_ -is [TagLib.Id3v2.UserTextInformationFrame] })) {
        $d = "$($fr.Description)"; $v = ($fr.Text -join '/')
        if (Test-HiddenLyricName $d) {
            if (Same (Get-RealLyrics $tf) ($fr.Text -join "`n")) { $id3.RemoveFrame($fr); $n++ }
            continue
        }
        if ($lyricsOnly) { continue }
        $real = switch -Regex ($d) {
            '^(major_brand|minor_version|compatible_brands)$' { $v; break }     # facts about the file it was converted from
            '^comment$' { $tf.Tag.Comment; break }
            '^T?BPM$' { $tf.Tag.BeatsPerMinute; break }
            '^(GROUPING|CONTENTGROUP)$' { $tf.Tag.Grouping; break }
            '^COMPILATION$' { if ($id3.IsCompilation) { '1' } else { '' }; break }
            '^(TRACKTOTAL|TOTALTRACKS)$' { $tf.Tag.TrackCount; break }
            '^(DISCTOTAL|TOTALDISCS)$' { $tf.Tag.DiscCount; break }
            '^ISRC$' { $tf.Tag.ISRC; break }
            '^(LABEL|ORGANIZATION|PUBLISHER)$' { $tf.Tag.Publisher; break }
            '^(ARTISTSORT|PERFORMERSORT)$' { , @($tf.Tag.PerformersSort); break }
            '^ALBUMARTISTSORT$' { , @($tf.Tag.AlbumArtistsSort); break }
            '^ALBUMSORT$' { $tf.Tag.AlbumSort; break }
            '^TITLESORT$' { $tf.Tag.TitleSort; break }
            '^COMPOSERSORT$' { , @($tf.Tag.ComposersSort); break }
            '^(ORIGINALDATE|ORIGINALYEAR)$' { $o = Get-OriginalYear $id3; if ($o -and $v.StartsWith($o)) { $v } else { '' }; break }
            '^CONDUCTOR$' { $tf.Tag.Conductor; break }
            '^LYRICIST$' { & $text 'TEXT'; break }
            '^(INITIALKEY|KEY)$' { $tf.Tag.InitialKey; break }
            '^SUBTITLE$' { $tf.Tag.Subtitle; break }
            '^REMIXER$' { $tf.Tag.RemixedBy; break }
            '^MUSICBRAINZ_ARTISTID$' { $tf.Tag.MusicBrainzArtistId; break }
            '^MUSICBRAINZ_ALBUMID$' { $tf.Tag.MusicBrainzReleaseId; break }
            '^MUSICBRAINZ_ALBUMARTISTID$' { $tf.Tag.MusicBrainzReleaseArtistId; break }
            '^MUSICBRAINZ_TRACKID$' { $tf.Tag.MusicBrainzTrackId; break }
            '^MUSICBRAINZ_RELEASEGROUPID$' { $tf.Tag.MusicBrainzReleaseGroupId; break }
            '^MUSICBRAINZ_DISCID$' { $tf.Tag.MusicBrainzDiscId; break }
            '^MUSICBRAINZ_ALBUMSTATUS$' { $tf.Tag.MusicBrainzReleaseStatus; break }
            '^MUSICBRAINZ_ALBUMTYPE$' { $tf.Tag.MusicBrainzReleaseType; break }
            '^RELEASECOUNTRY$' { $tf.Tag.MusicBrainzReleaseCountry; break }
            default { $null }
        }
        if ((Text $real) -and (Same (Text $real) $v)) { $id3.RemoveFrame($fr); $n++ }
    }
    $n
}
# Every entry of an MP3's tag except the lyrics, as text that is equal exactly when the entries are: for
# proving that a lyrics change touched nothing else.
function Get-OtherFrames([string]$path) {
    $f = [TagLib.File]::Create($path)
    try {
        $id3 = $f.GetTag([TagLib.TagTypes]::Id3v2, $false)
        if (-not $id3) { return , @() }
        $out = foreach ($fr in $id3.GetFrames()) {
            $id = $fr.FrameId.ToString()
            if ($id -in 'USLT', 'SYLT') { continue }
            if ($fr -is [TagLib.Id3v2.UserTextInformationFrame] -and (Test-HiddenLyricName "$($fr.Description)")) { continue }
            $id + ':' + [Convert]::ToBase64String($fr.Render($id3.Version).Data)
        }
        , @($out | Sort-Object)
    } finally { $f.Dispose() }
}

# Applies one declarative tag change to a file, safely. $step: File, Set (hashtable), Junk (list of [field,value]),
# Cover (image path), CoverMode ('all' | 'missing'). Returns $null on success or a reason it was skipped.
function Invoke-TagStep($b, $step) {
    $path = $step.File
    if (-not (Test-Path -LiteralPath $path)) { return 'file no longer exists' }
    if (Test-Locked $path) { return 'file is open in another program' }
    $dirty = Test-Id3Clean $path
    if ($dirty) { return $dirty }
    $tmp = New-TempPath $path
    Copy-Item -LiteralPath $path -Destination $tmp -Force
    try {
        try { $h1 = Get-AudioBytesHash $path } catch { return "its tags are malformed, so it was left alone ($($_.Exception.Message))" }
        $droppedStacked = $false
        if ($step.Junk -and $path.EndsWith('.mp3', [StringComparison]::OrdinalIgnoreCase)) {
            # junk in a second, stacked tag: remove that tag - but only if everything in it is junk or a copy of the main tag
            $bytes = [IO.File]::ReadAllBytes($tmp)
            $stk = Get-StackedId3 $bytes
            if ($stk) {
                $probe = [TagLib.File]::Create($tmp)
                try {
                    $mainFrames = @($probe.GetTag([TagLib.TagTypes]::Id3v2, $false).GetFrames())
                    $mainText = @($mainFrames | ForEach-Object { try { ($_.Text -join '/').Trim() } catch { } } | Where-Object { $_ })
                    $mainIds = @($mainFrames | ForEach-Object { $_.FrameId.ToString() } | Select-Object -Unique)
                } finally { $probe.Dispose() }
                $junkVals = @($step.Junk | ForEach-Object { "$($_[1])".Trim() })      # Junk is a list of (field, value)
                # players read the main tag and ignore a second one, so a field the main tag already has adds nothing;
                # only a kind of field the main tag lacks entirely counts as information worth keeping
                $unique = @($stk.Frames | Where-Object { $_.Text -and $junkVals -notcontains $_.Text -and $mainText -notcontains $_.Text -and
                                                        $mainIds -notcontains $_.Id -and $_.Id -notin 'TSSE', 'TENC', 'COMM', 'TXXX' })
                if ($unique) { return "it has a second tag holding other info ($(($unique | Select-Object -First 2 | ForEach-Object { "$($_.Id) '$(Short $_.Text 30)'" }) -join ', ')), so it was left alone" }
                $out = [IO.MemoryStream]::new()
                $out.Write($bytes, 0, $stk.Start); $out.Write($bytes, $stk.Start + $stk.Length, $bytes.Length - $stk.Start - $stk.Length)
                [IO.File]::WriteAllBytes($tmp, $out.ToArray())
                $droppedStacked = $true
            }
        }
        $others = $null
        if ($step.Exact) {
            # a lyrics-only change that keeps no copy of the old file: it must be an MP3, and every other entry
            # of its tag is compared before and after
            if (-not $path.EndsWith('.mp3', [StringComparison]::OrdinalIgnoreCase)) { return 'only an MP3''s tag can be checked entry by entry, so it was left alone' }
            try { $others = Get-OtherFrames $path } catch { return "its tags are malformed, so it was left alone ($($_.Exception.Message))" }
        }
        try { $tf = [TagLib.File]::Create($tmp) } catch { return "its tags are malformed, so it was left alone ($($_.Exception.Message))" }
        try {
            $want = @{}
            $lyricsBefore = Get-RealLyrics $tf
            if ($step.CarryFrom) {
                $carried = Copy-Tags $tf $step.CarryFrom $step.Skip ([bool]$step.DropPersonal)
                foreach ($k in $carried.Keys) { $want[$k] = $carried[$k] }
                # ffmpeg writes a FLAC's track as "3/12" in one entry; a FLAC keeps the number and the total
                # apart, and some players read nothing else (the same for the disc)
                $fx = $null
                if ($tf -is [TagLib.Flac.File]) { $fx = $tf.GetTag([TagLib.TagTypes]::Xiph, $false) }
                if ($fx -and "$($fx.GetFirstField('TRACKNUMBER'))" -match '/') {
                    $no = $tf.Tag.Track; $of = $tf.Tag.TrackCount
                    $tf.Tag.TrackCount = $of; $tf.Tag.Track = $no; $want['track'] = "$no"; $want['trackcount'] = "$of"
                }
                if ($fx -and "$($fx.GetFirstField('DISCNUMBER'))" -match '/') {
                    $no = $tf.Tag.Disc; $of = $tf.Tag.DiscCount
                    $tf.Tag.DiscCount = $of; $tf.Tag.Disc = $no; $want['disc'] = "$no"; $want['disccount'] = "$of"
                }
            }
            foreach ($k in $step.Set.Keys) {
                $v = $step.Set[$k]
                switch ($k) {
                    'album'        { $tf.Tag.Album = $v }
                    'album_artist' { $tf.Tag.AlbumArtists = [string[]]@($v) }
                    'artist'       { $tf.Tag.Performers = [string[]]@($v) }
                    'title'        { $tf.Tag.Title = $v }
                    'track'        { $tf.Tag.Track = [uint32]$v }
                    'disc'         { $tf.Tag.Disc = [uint32]$v }
                    'year'         { $tf.Tag.Year = [uint32]$v }
                    'genre'        { $tf.Tag.Genres = [string[]]@($v) }
                    'trackcount'   { $tf.Tag.TrackCount = [uint32]$v }
                    'disccount'    { $tf.Tag.DiscCount = [uint32]$v }
                    'compilation'  {
                        # (a FLAC file says it in its own tag: asking one for an ID3 tag would put one in front of the file)
                        if ($tf -is [TagLib.Flac.File]) { $tf.GetTag([TagLib.TagTypes]::Xiph, $true).SetField('COMPILATION', [string[]]@('1')) }
                        else {
                            $ct = $tf.GetTag([TagLib.TagTypes]::Id3v2, $true); if (-not $ct) { $ct = $tf.GetTag([TagLib.TagTypes]::Apple, $true) }
                            if ($ct) { $ct.IsCompilation = $true }
                        }
                    }
                }
                $want[$k] = "$v"
            }
            $removed = 0
            if ($step.Junk) {
                $pairs = @(Get-JunkPairs $tf)
                foreach ($j in $step.Junk) {
                    # match the exact value; the ID3v1 copy is cut to 28-30 characters, so it matches by prefix
                    foreach ($p in $pairs | Where-Object { $_.Key -eq ($j[0] -replace '^comments$', 'comment') -and ((Same $_.Value $j[1]) -or
                               ($_.Tag -is [TagLib.Id3v1.Tag] -and $_.Value.Length -ge 20 -and $j[1].StartsWith($_.Value.Trim()))) }) {
                        if ($p.Frame) { $p.Tag.RemoveFrame($p.Frame) }
                        elseif ($p.Field) { $p.Tag.RemoveField($p.Field) }
                        elseif ($p.Apple) { $p.Tag.($p.Apple) = $null }
                        $removed++
                    }
                }
            }
            if ($step.Cover) {
                $hasFront = @($tf.Tag.Pictures | Where-Object { $_.Type -in 'FrontCover', 'Other' }).Count -gt 0
                if ($step.CoverMode -eq 'all' -or -not $hasFront) {
                    $pic = [TagLib.Picture]::new($step.Cover); $pic.Type = 'FrontCover'
                    $keep = @($tf.Tag.Pictures | Where-Object { $_.Type -notin 'FrontCover', 'Other' })   # keep back covers, booklets etc.
                    $tf.Tag.Pictures = [TagLib.IPicture[]](@($pic) + $keep)
                    $want['cover'] = '1'
                }
            }
            # lyrics: out of a tag that hides them, then what the plan says (never over words that are there, unless told to)
            $stood = 0
            # (the text tags a conversion left are taken out while the fields still hold what was carried:
            # each goes only when its field has the same value)
            if ($step.CarryFrom) { $stood += Remove-StandIns $tf $false }
            # a download: what another person's player left in it goes (their rating, play count, DJ data)
            if ($step.DropPersonal) { $stood += Remove-Personal $tf }
            if ($step.Unhide -and -not (Get-RealLyrics $tf)) {
                $hidden = Get-HiddenLyrics $tf
                if ($hidden.Trim()) { $tf.Tag.Lyrics = $hidden; $want['lyrics'] = $hidden }
            }
            if ($step.Unhide) { $stood += Remove-StandIns $tf $true }
            # undoing a lyrics repair: only a file whose lyrics are still what the repair wrote is put back
            if ($null -ne $step.OnlyIf) {
                $now = Get-RealLyrics $tf
                $target = if ($step.LyricsMode -eq 'clear') { '' } else { "$($step.Lyrics)" }
                if (Same $now $target) { return 'nothing needed changing' }
                if (-not (Same $now "$($step.OnlyIf)")) { return 'its lyrics were changed after the repair, so they were left as they are' }
            }
            $forget = { foreach ($k in 'carry lyrics', 'frames USLT', 'frames SYLT') { $want.Remove($k) } }
            if ($step.LyricsMode -eq 'clear') {
                if (Get-RealLyrics $tf) { Clear-Lyrics $tf; $want['lyrics'] = ''; & $forget }
            } elseif ("$($step.Lyrics)".Trim() -and ($step.LyricsMode -eq 'set' -or -not (Get-RealLyrics $tf))) {
                if (-not (Same (Get-RealLyrics $tf) $step.Lyrics)) {
                    if (Get-RealLyrics $tf) { Clear-Lyrics $tf }      # the old words go, with any timed copy of them
                    $tf.Tag.Lyrics = "$($step.Lyrics)"; $want['lyrics'] = "$($step.Lyrics)"; & $forget
                }
            }
            if (-not $want.Count -and -not $removed -and -not $droppedStacked -and -not $stood) { return 'nothing needed changing' }
            if ($want.ContainsKey('lyrics')) { $step.Old = $lyricsBefore }
            # (the tag library adds the old short tag at a file's end when it saves; a change that must touch
            # nothing else does not add one to a file that had none)
            if ($null -ne $others -and (Read-Id3v1Block ([IO.File]::ReadAllBytes($path))).Length -eq 0) { $tf.RemoveTags([TagLib.TagTypes]::Id3v1) }
            $tf.Save()
        } finally { $tf.Dispose() }

        # --- verify the copy before it replaces anything: the audio bytes must be exactly the same ---
        $h2 = Get-AudioBytesHash $tmp
        if (-not $h1 -or $h1 -ne $h2) { return 'the audio data would have changed, so nothing was saved' }
        $chk = [TagLib.File]::Create($tmp)
        try {
            if ($step.DropPersonal -and (Remove-Personal $chk $false)) { return "another person's rating or DJ data would have stayed in it, so nothing was saved" }
            $carriers = @(Get-Carriers) + @(Get-PlainCarriers)
            $cid3 = $chk.GetTag([TagLib.TagTypes]::Id3v2, $false)
            foreach ($k in $want.Keys) {
                # what was carried over, and the lyrics: compared as text, however the line ends are written
                if ($k -eq 'lyrics') { if (-not (Same (Get-RealLyrics $chk) $want[$k])) { return "the lyrics didn't stick, so nothing was saved" }; continue }
                if ($k.StartsWith('carry ')) {
                    $name = $k.Substring(6)
                    $now = if ($name -eq 'compilation') { if ($cid3 -and $cid3.IsCompilation) { '1' } else { '' } } else { $c = $carriers | Where-Object { $_.N -eq $name }; $has = if ($c.Has) { $c.Has } else { $c.Read }; Text (& $has $chk) }
                    if (-not (Same $now $want[$k])) { return "the $name didn't carry over, so nothing was saved" }
                    continue
                }
                if ($k -eq 'original year') { if ((Get-OriginalYear $cid3) -ne $want[$k]) { return "the original year didn't carry over, so nothing was saved" }; continue }
                if ($k.StartsWith('text ')) { if (-not (Same "$($cid3.GetTextAsString((Id3Name $k.Substring(5))))" $want[$k])) { return "the change to $k didn't stick, so nothing was saved" }; continue }
                if ($k.StartsWith('frames ')) {
                    $id = $k.Substring(7); $n = @($cid3.GetFrames() | Where-Object { $_.FrameId.ToString() -eq $id }).Count
                    if ($n -lt [int]$want[$k]) { return "the $id entries didn't carry over, so nothing was saved" }
                    continue
                }
                $got = switch ($k) {
                    'album' { $chk.Tag.Album } 'album_artist' { $chk.Tag.FirstAlbumArtist } 'artist' { $chk.Tag.FirstPerformer }
                    'title' { $chk.Tag.Title } 'track' { "$($chk.Tag.Track)" } 'disc' { "$($chk.Tag.Disc)" }
                    'year' { "$($chk.Tag.Year)" } 'genre' { $chk.Tag.FirstGenre } 'cover' { if ($chk.Tag.Pictures.Count) { '1' } else { '0' } }
                    'trackcount' { "$($chk.Tag.TrackCount)" } 'disccount' { "$($chk.Tag.DiscCount)" }
                    'compilation' {
                        if ($chk -is [TagLib.Flac.File]) { $cx = $chk.GetTag([TagLib.TagTypes]::Xiph, $false); if ($cx -and "$($cx.GetFirstField('COMPILATION'))".Trim() -eq '1') { '1' } else { '0' } }
                        else { $ct = $chk.GetTag([TagLib.TagTypes]::Id3v2, $false); if (-not $ct) { $ct = $chk.GetTag([TagLib.TagTypes]::Apple, $false) }; if ($ct -and $ct.IsCompilation) { '1' } else { '0' } }
                    }
                }
                if ("$got" -ne $want[$k]) { return "the change to $k didn't stick, so nothing was saved" }
            }
        } finally { $chk.Dispose() }
        if ($null -ne $others) {
            $after = Get-OtherFrames $tmp
            if ($others.Count -ne $after.Count -or ($others.Count -gt 0 -and (Compare-Object $others $after))) { return 'another entry of its tag would have changed, so nothing was saved' }
            $v1 = Read-Id3v1Block ([IO.File]::ReadAllBytes($path)); $v1b = Read-Id3v1Block ([IO.File]::ReadAllBytes($tmp))
            if ([Convert]::ToBase64String($v1) -ne [Convert]::ToBase64String($v1b)) { return 'the old tag at its end would have changed, so nothing was saved' }
        }
        Complete-Swap $b $path $tmp $step.Reason
        $null
    } finally {
        if (Test-Path -LiteralPath $tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
    }
}

# ---------------- byte-level fixes that keep every tag exactly as it was ----------------
function Read-Id3v2Block([byte[]]$bytes) {     # the ID3v2 tag at the start (with footer if any), or empty
    if ($bytes.Length -lt 10 -or $bytes[0] -ne 0x49 -or $bytes[1] -ne 0x44 -or $bytes[2] -ne 0x33) { return , [byte[]]@() }
    $size = (([int]$bytes[6] -band 0x7f) -shl 21) -bor (([int]$bytes[7] -band 0x7f) -shl 14) -bor (([int]$bytes[8] -band 0x7f) -shl 7) -bor ([int]$bytes[9] -band 0x7f)
    $len = 10 + $size + $(if ($bytes[5] -band 0x10) { 10 } else { 0 })
    , [byte[]]$bytes[0..([math]::Min($len, $bytes.Length) - 1)]
}
function Read-Id3v1Block([byte[]]$bytes) {
    if ($bytes.Length -gt 128 -and $bytes[$bytes.Length - 128] -eq 0x54 -and $bytes[$bytes.Length - 127] -eq 0x41 -and $bytes[$bytes.Length - 126] -eq 0x47) {
        return , [byte[]]$bytes[($bytes.Length - 128)..($bytes.Length - 1)]
    }
    , [byte[]]@()
}
# "MP3 inside a WAV wrapper": keep the tag, take the raw MP3 frames out of the WAV "data" chunk
function Read-ApeBlock([byte[]]$bytes, [int]$v1Len) {
    $end = $bytes.Length - $v1Len
    if ($end -lt 32 -or [Text.Encoding]::ASCII.GetString($bytes, $end - 32, 8) -ne 'APETAGEX') { return , [byte[]]@() }
    $size = [BitConverter]::ToUInt32($bytes, $end - 32 + 12)            # items + footer
    $hasHeader = ([BitConverter]::ToUInt32($bytes, $end - 32 + 20) -band 0x80000000) -ne 0
    $len = [long]$size + $(if ($hasHeader) { 32 } else { 0 })
    if ($len -le 0 -or $len -gt $end) { return , [byte[]]@() }
    , [byte[]]$bytes[($end - $len)..($end - 1)]
}
# missing VBR header: let ffmpeg rebuild the audio part with a proper header, then put the ORIGINAL tag bytes back on
# (the ID3v2 tag in front, and any APE and ID3v1 tags at the end)
function Invoke-VbrHeaderStep($b, $step) {
    $path = $step.File
    if (Test-Locked $path) { return 'file is open in another program' }
    $bytes = [IO.File]::ReadAllBytes($path)
    $id3 = Read-Id3v2Block $bytes; $v1 = Read-Id3v1Block $bytes; $ape = Read-ApeBlock $bytes $v1.Length
    $tmp = New-TempPath $path; $raw = "$tmp.audio.mp3"; $bare = "$tmp.bare.mp3"
    try {
        # the original without its end tags: ffmpeg would otherwise read the APE tag's bytes as broken audio frames
        $mid = $bytes.Length - $v1.Length - $ape.Length
        $fs = [IO.File]::Create($bare); try { $fs.Write($bytes, 0, $mid) } finally { $fs.Dispose() }
        & $ffmpeg -hide_banner -nostdin -v error -y -i $bare -map 0:a -c:a copy -map_metadata -1 -id3v2_version 0 -write_id3v1 0 -write_xing 1 -f mp3 -- $raw 2>$null
        if (-not (Test-Path -LiteralPath $raw)) { return 'ffmpeg couldn''t rebuild the file' }
        $audio = [IO.File]::ReadAllBytes($raw)
        $out = [IO.MemoryStream]::new(); $out.Write($id3, 0, $id3.Length); $out.Write($audio, 0, $audio.Length)
        if ($ape.Length) { $out.Write($ape, 0, $ape.Length) }; if ($v1.Length) { $out.Write($v1, 0, $v1.Length) }
        [IO.File]::WriteAllBytes($tmp, $out.ToArray())
        # same audio as before: compare the frames alone (end tags off both)
        $tb = [IO.File]::ReadAllBytes($tmp); $cmp = "$tmp.cmp.mp3"
        [IO.File]::WriteAllBytes($cmp, $tb[0..($tb.Length - $v1.Length - $ape.Length - 1)])
        $same = (Get-AudioHash $bare) -eq (Get-AudioHash $cmp); Remove-Item -LiteralPath $cmp -Force -ErrorAction SilentlyContinue
        if (-not $same) { return 'the rebuilt file didn''t match the original audio, so nothing was saved' }
        Complete-Swap $b $path $tmp $step.Reason
        $null
    } finally { foreach ($f in $tmp, $raw, $bare) { if (Test-Path -LiteralPath $f) { Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue } } }
}

# ---------------- moving and removing ----------------

# ---------------- the run ----------------
if ($Functions) { return }
if (-not $Plan -or -not $Out -or -not $Ffmpeg) { throw 'TagEditor.ps1 needs -Plan, -Out and -Ffmpeg' }
$me = $PSCommandPath
$steps = @(Get-Content -LiteralPath $Plan -Raw -Encoding utf8 | ConvertFrom-Json)
$results = $steps | ForEach-Object -ThrottleLimit 4 -Parallel {
    $s = $_
    $ffmpeg = $using:Ffmpeg
    # Loaded for every file, not only where the functions are missing: a worker that is used for a second
    # file still has the functions, but not what the script sets at its top (the table of frames to carry,
    # stopping at the first error). From the fifth file of a plan on, those were empty.
    . $using:me -Functions
    $set = @{}; if ($s.Set) { foreach ($p in $s.Set.PSObject.Properties) { $set[$p.Name] = "$($p.Value)" } }
    $junk = @(foreach ($j in @($s.Junk)) { if ($j) { , @("$($j[0])", "$($j[1])") } })
    $step = [pscustomobject]@{ Type = 'tags'; File = $s.File; Set = $set; Junk = $junk; Cover = $s.Cover; CoverMode = $s.CoverMode; Reason = 'import'
        CarryFrom = $s.CarryFrom; Skip = @(foreach ($k in @($s.Skip)) { if ($k) { "$k" } }); Lyrics = $s.Lyrics; LyricsMode = "$($s.LyricsMode)"; Unhide = [bool]$s.Unhide; Exact = [bool]$s.Exact; OnlyIf = $s.OnlyIf; DropPersonal = [bool]$s.DropPersonal; Old = $null }
    $why = try { Invoke-TagStep $null $step } catch { "$($_.Exception.Message)" }
    $ok = (-not $why) -or $why -eq 'nothing needed changing'
    $note = ''
    if ($ok -and $s.Vbr) {
        $v = try { Invoke-VbrHeaderStep $null ([pscustomobject]@{ File = $s.File; Reason = 'import' }) } catch { "$($_.Exception.Message)" }
        $note = if (-not $v) { 'added the missing length header' } else { "length header not fixed: $v" }
    }
    [pscustomobject]@{ File = $s.File; Ok = $ok; Why = "$why"; Note = $note; OldLyrics = $(if ($null -ne $step.Old -and -not $why) { "$($step.Old)" } else { $null }) }
}
ConvertTo-Json @($results) -Depth 3 | Set-Content -LiteralPath $Out -Encoding utf8
