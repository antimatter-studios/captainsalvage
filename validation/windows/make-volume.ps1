# Windows formats an NTFS volume, fills it with a deterministic tree, and records what it holds.
# Run by validation/windows/capture.sh INSIDE the Windows test guest (Windows PowerShell 5.1, as an
# Administrator). Nothing here is the app or a driver: the formatter is Format-Volume, the writer
# is Windows' own file system, and the expected result is what Get-ChildItem and Get-FileHash
# report about it afterwards.
#
#   make-volume.ps1 -Name N -SizeMB S -ClusterBytes C [-SectorBytes 512|4096] [-PartitionStyle MBR|GPT]
#                   [-Scale small|large] -Out DIR
#
# Writes into DIR:
#   N.img.gz         the NTFS volume's bytes (the partition, not the disk), gzip compressed
#   N.manifest.tsv   what Windows reports, one line per thing (see "The manifest" below)
#   N.info.txt       Windows version, fsutil ntfsinfo, disk geometry and the read-only chkdsk verdict
#
# The manifest, tab separated, paths relative to the volume root with `/` between names:
#   m <key> <value>                  facts about the volume and the guest
#   d <path>                         a folder Get-ChildItem lists
#   f <size> <sha256> <path>         a file: Length, and Get-FileHash of its default stream
#   s <size> <sha256> <path>:<name>  an alternate data stream of a file or folder
#   g <size> <sha256> <path>         a file that was deleted last, after its hash was taken
#   p <0|1> <path>                   1 when the first 32 bytes of that deleted file are still in the volume
#   e <extents> <path>               how many extents fsutil file queryExtents reports for a file
#   a <attributes> <path>            Hidden, System, ReadOnly, SparseFile, Compressed ... of an entry
#   n <short name> <path>            the 8.3 name Windows made for an entry
#   l <kind> <path>                  a reparse point (junction, symlink): listed, not followed
#   h <path> <path>                  the second path is a hard link to the same file as the first
#   u <usn> <reason> <file> <parent> <attributes> <time> <name>
#                                    a record of the change journal, read by Windows (FSCTL_READ_USN_JOURNAL): the USN, the
#                                    reason flags as a number, the file and parent references as numbers (sequence in the top
#                                    16 bits), the attributes, the FILETIME and the name; every record, in USN order
#   x <path>                         a folder Windows owns (System Volume Information): the folder
#                                    is expected, what is inside it is not part of the check
#
# Everything is made from fixed seeds, so the same parameters give the same content. The volume
# serial number and the timestamps are Windows' own and differ between runs; the manifest does
# not depend on them.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Name,
    [Parameter(Mandatory)][int]$SizeMB,
    [Parameter(Mandatory)][int]$ClusterBytes,
    [int]$SectorBytes = 512,
    [ValidateSet('MBR', 'GPT')][string]$PartitionStyle = 'MBR',
    [ValidateSet('small', 'large')][string]$Scale = 'small',
    [Parameter(Mandatory)][string]$Out
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2

Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using Microsoft.Win32.SafeHandles;

public static class Prng {
    // Deterministic bytes from a seed: xorshift128+ keyed by the seed's SHA-256.
    public static byte[] Bytes(string seed, int length) {
        byte[] key;
        using (var sha = SHA256.Create()) { key = sha.ComputeHash(Encoding.UTF8.GetBytes(seed)); }
        ulong s0 = BitConverter.ToUInt64(key, 0) | 1UL, s1 = BitConverter.ToUInt64(key, 8) ^ 0x9E3779B97F4A7C15UL;
        var result = new byte[length];
        int at = 0;
        while (at < length) {
            ulong x = s0, y = s1;
            s0 = y;
            x ^= x << 23;
            s1 = x ^ y ^ (x >> 17) ^ (y >> 26);
            ulong value = s1 + y;
            for (int i = 0; i < 8 && at < length; i++) { result[at++] = (byte)(value >> (8 * i)); }
        }
        return result;
    }
}

public static class Streams {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct FindStreamData {
        public long StreamSize;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 296)] public string StreamName;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr FindFirstStreamW(string file, int level, out FindStreamData data, int flags);
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern bool FindNextStreamW(IntPtr find, out FindStreamData data);
    [DllImport("kernel32.dll")]
    static extern bool FindClose(IntPtr find);

    // The named data streams of a file or a folder, as "name<TAB>length" lines. Get-Item -Stream does
    // not list the streams of a folder, and Windows' own FindFirstStream does.
    public static string[] Of(string path) {
        var found = new System.Collections.Generic.List<string>();
        FindStreamData data;
        IntPtr find = FindFirstStreamW(path, 0, out data, 0);
        if (find == new IntPtr(-1)) { return found.ToArray(); }
        try {
            do {
                // ":name:$DATA", or "::$DATA" for the file's own contents.
                var parts = data.StreamName.Split(':');
                if (parts.Length == 3 && parts[1].Length > 0) { found.Add(parts[1] + "\t" + data.StreamSize); }
            } while (FindNextStreamW(find, out data));
        } finally { FindClose(find); }
        return found.ToArray();
    }
}

public static class ShortName {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern uint GetShortPathName(string path, StringBuilder buffer, uint size);

    // The 8.3 name Windows made for the last component of a path, or null when it has none.
    public static string Of(string path) {
        var buffer = new StringBuilder(1024);
        uint n = GetShortPathName(path, buffer, (uint)buffer.Capacity);
        if (n == 0 || n >= buffer.Capacity) { return null; }
        var result = buffer.ToString();
        return result.Substring(result.LastIndexOf('\\') + 1);
    }
}

public static class Usn {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security,
        uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool DeviceIoControl(SafeFileHandle handle, uint code, byte[] input, uint inputSize,
        byte[] output, uint outputSize, out uint returned, IntPtr overlapped);

    const uint FSCTL_QUERY_USN_JOURNAL = 0x000900F4, FSCTL_READ_USN_JOURNAL = 0x000900BB;

    // Every record the change journal of a volume (`\\.\X:`) holds, as Windows itself reads them, one line each:
    // usn, reason flags, file reference, parent reference, file attributes, FILETIME and name, tab separated.
    public static string[] Read(string volume) {
        const uint GENERIC_READ = 0x80000000, SHARE_RW = 3, OPEN_EXISTING = 3;
        var handle = CreateFile(volume, GENERIC_READ, SHARE_RW, IntPtr.Zero, OPEN_EXISTING, 0, IntPtr.Zero);
        if (handle.IsInvalid) { throw new IOException("open " + volume + ": " + Marshal.GetLastWin32Error()); }
        var lines = new System.Collections.Generic.List<string>();
        using (handle) {
            var state = new byte[64];
            uint returned;
            if (!DeviceIoControl(handle, FSCTL_QUERY_USN_JOURNAL, null, 0, state, (uint)state.Length, out returned, IntPtr.Zero)) {
                throw new IOException("query the change journal: " + Marshal.GetLastWin32Error());
            }
            ulong journalId = BitConverter.ToUInt64(state, 0);
            long first = BitConverter.ToInt64(state, 8);
            long next = BitConverter.ToInt64(state, 16);
            long cursor = first;
            var buffer = new byte[64 * 1024];
            while (cursor < next) {
                var request = new byte[40];
                BitConverter.GetBytes(cursor).CopyTo(request, 0);
                BitConverter.GetBytes(0xFFFFFFFFu).CopyTo(request, 8);   // every reason
                BitConverter.GetBytes(0u).CopyTo(request, 12);           // not only on close
                BitConverter.GetBytes(0UL).CopyTo(request, 16);          // no timeout
                BitConverter.GetBytes(0UL).CopyTo(request, 24);          // no bytes to wait for
                BitConverter.GetBytes(journalId).CopyTo(request, 32);
                if (!DeviceIoControl(handle, FSCTL_READ_USN_JOURNAL, request, (uint)request.Length, buffer, (uint)buffer.Length, out returned, IntPtr.Zero)) {
                    throw new IOException("read the change journal at " + cursor + ": " + Marshal.GetLastWin32Error());
                }
                if (returned <= 8) { break; }
                long following = BitConverter.ToInt64(buffer, 0);
                int at = 8;
                while (at + 60 <= returned) {
                    int length = BitConverter.ToInt32(buffer, at);
                    if (length < 60 || at + length > returned) { break; }
                    if (BitConverter.ToUInt16(buffer, at + 4) == 2) {
                        int nameLength = BitConverter.ToUInt16(buffer, at + 56), nameOffset = BitConverter.ToUInt16(buffer, at + 58);
                        lines.Add(string.Join("\t", new string[] {
                            BitConverter.ToInt64(buffer, at + 24).ToString(),
                            BitConverter.ToUInt32(buffer, at + 40).ToString(),
                            BitConverter.ToUInt64(buffer, at + 8).ToString(),
                            BitConverter.ToUInt64(buffer, at + 16).ToString(),
                            BitConverter.ToUInt32(buffer, at + 52).ToString(),
                            BitConverter.ToInt64(buffer, at + 32).ToString(),
                            Encoding.Unicode.GetString(buffer, at + nameOffset, nameLength)
                        }));
                    }
                    at += length;
                }
                if (following <= cursor) { break; }
                cursor = following;
            }
        }
        return lines.ToArray();
    }
}

public static class RawDisk {
    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    static extern SafeFileHandle CreateFile(string name, uint access, uint share, IntPtr security,
        uint disposition, uint flags, IntPtr template);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetFilePointerEx(SafeFileHandle handle, long distance, out long newPosition, uint method);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool ReadFile(SafeFileHandle handle, byte[] buffer, uint count, out uint read, IntPtr overlapped);

    // Copies `length` bytes of a physical drive from `offset` into a gzip file. Both are multiples of
    // the sector size, and so is every read. Reports which of the probes (byte strings) occur anywhere
    // in what was copied.
    public static bool[] Dump(string device, long offset, long length, string target, byte[][] probes) {
        const uint GENERIC_READ = 0x80000000, SHARE_RW = 3, OPEN_EXISTING = 3;
        var found = new bool[probes.Length];
        var handle = CreateFile(device, GENERIC_READ, SHARE_RW, IntPtr.Zero, OPEN_EXISTING, 0, IntPtr.Zero);
        if (handle.IsInvalid) { throw new IOException("open " + device + ": " + Marshal.GetLastWin32Error()); }
        using (handle)
        using (var file = File.Create(target))
        using (var gzip = new System.IO.Compression.GZipStream(file, System.IO.Compression.CompressionLevel.Optimal)) {
            long position;
            if (!SetFilePointerEx(handle, offset, out position, 0)) { throw new IOException("seek: " + Marshal.GetLastWin32Error()); }
            int keep = 0;
            foreach (var probe in probes) { keep = Math.Max(keep, probe.Length - 1); }
            var buffer = new byte[keep + 4 * 1024 * 1024];
            long left = length;
            int carried = 0;
            while (left > 0) {
                int want = (int)Math.Min(buffer.Length - keep, left);
                int got = 0;
                while (got < want) {
                    var chunk = new byte[want - got];
                    uint n;
                    if (!ReadFile(handle, chunk, (uint)chunk.Length, out n, IntPtr.Zero) || n == 0) {
                        throw new IOException("read at " + (length - left + got) + ": " + Marshal.GetLastWin32Error());
                    }
                    Buffer.BlockCopy(chunk, 0, buffer, keep + got, (int)n);
                    got += (int)n;
                }
                gzip.Write(buffer, keep, want);
                int end = keep + want;
                for (int p = 0; p < probes.Length; p++) {
                    if (found[p]) { continue; }
                    var probe = probes[p];
                    for (int i = keep - carried; i + probe.Length <= end; i++) {
                        if (buffer[i] != probe[0]) { continue; }
                        int k = 1;
                        while (k < probe.Length && buffer[i + k] == probe[k]) { k++; }
                        if (k == probe.Length) { found[p] = true; break; }
                    }
                }
                carried = Math.Min(keep, end - keep);
                Buffer.BlockCopy(buffer, end - carried, buffer, keep - carried, carried);
                left -= want;
            }
        }
        return found;
    }
}
'@

$ProgressPreference = 'SilentlyContinue'
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path -LiteralPath $Out).Path
$utf8 = New-Object System.Text.UTF8Encoding($false)

# ---------------------------------------------------------------- the disk
function Invoke-Diskpart([string[]]$Lines, [string]$Dir) {
    $script = Join-Path $Dir 'diskpart.txt'
    [IO.File]::WriteAllLines($script, $Lines, [Text.Encoding]::ASCII)
    $text = & diskpart.exe /s $script 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "diskpart failed: $text" }
}

# A VHDX records its sector sizes in two metadata items. diskpart only makes 512-byte disks, so a
# 4096-byte-sector disk is a VHDX whose two items were rewritten before it was ever attached.
# (The metadata region carries no checksum; Get-Disk on the attached image proves the result.)
function Set-VhdxSectorSize([string]$Path, [int]$Bytes) {
    $items = @{
        ([guid]'8141BF1D-A96F-4709-BA47-F233A8FAAB5F') = $Bytes   # logical
        ([guid]'CDA348C7-445D-4471-9CC9-E9885251C556') = $Bytes   # physical
    }
    $metadataRegion = [guid]'8B7CA206-4790-4B9A-B8FE-575F050F886E'
    $stream = [IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')
    try {
        $table = New-Object byte[] 65536
        $null = $stream.Seek(196608, 'Begin'); $null = $stream.Read($table, 0, 65536)
        if ([Text.Encoding]::ASCII.GetString($table, 0, 4) -cne 'regi') { throw 'VHDX region table signature missing' }
        $metadataOffset = $null
        for ($i = 0; $i -lt [BitConverter]::ToUInt32($table, 8); $i++) {
            $at = 16 + 32 * $i
            if ([guid]::new([byte[]]$table[$at..($at + 15)]) -eq $metadataRegion) {
                $metadataOffset = [BitConverter]::ToInt64($table, $at + 16)
            }
        }
        if ($null -eq $metadataOffset) { throw 'VHDX metadata region missing' }
        $meta = New-Object byte[] 65536
        $null = $stream.Seek($metadataOffset, 'Begin'); $null = $stream.Read($meta, 0, 65536)
        if ([Text.Encoding]::ASCII.GetString($meta, 0, 8) -cne 'metadata') { throw 'VHDX metadata table signature missing' }
        $patched = 0
        for ($i = 0; $i -lt [BitConverter]::ToUInt16($meta, 10); $i++) {
            $at = 32 + 32 * $i
            $item = [guid]::new([byte[]]$meta[$at..($at + 15)])
            if ($items.ContainsKey($item)) {
                $null = $stream.Seek($metadataOffset + [BitConverter]::ToUInt32($meta, $at + 16), 'Begin')
                $stream.Write([BitConverter]::GetBytes([uint32]$items[$item]), 0, 4)
                $patched++
            }
        }
        if ($patched -ne 2) { throw "found $patched of the 2 VHDX sector size items" }
    } finally { $stream.Dispose() }
}

$work = Join-Path $Out ("work-" + $Name)
Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $work | Out-Null
$ext = if ($SectorBytes -eq 512) { 'vhd' } else { 'vhdx' }
$disk = Join-Path $work "$Name.$ext"
$attached = $false
$letter = $null
$diskTool = "diskpart create vdisk maximum=$SizeMB type=expandable ($ext)"
if ($SectorBytes -ne 512) { $diskTool += "; VHDX logical and physical sector size items set to $SectorBytes" }

try {
    Invoke-Diskpart @("create vdisk file=`"$disk`" maximum=$SizeMB type=expandable", 'exit') $work
    if ($SectorBytes -ne 512) { Set-VhdxSectorSize $disk $SectorBytes }
    $attached = $true
    $number = (Mount-DiskImage -ImagePath $disk -PassThru | Get-Disk).Number
    $diskInfo = Get-Disk -Number $number
    if ($diskInfo.LogicalSectorSize -ne $SectorBytes) {
        throw "the scratch disk has $($diskInfo.LogicalSectorSize)-byte logical sectors, not $SectorBytes"
    }
    Initialize-Disk -Number $number -PartitionStyle $PartitionStyle | Out-Null
    $partition = New-Partition -DiskNumber $number -UseMaximumSize -AssignDriveLetter
    $letter = [string]$partition.DriveLetter
    if (-not $letter) { throw 'the scratch partition has no drive letter' }
    Format-Volume -DriveLetter $letter -FileSystem NTFS -AllocationUnitSize $ClusterBytes `
        -NewFileSystemLabel 'WINORACLE' -Force -Confirm:$false | Out-Null
    # Short (8.3) names are off by default on a volume that is not the system's; the tree wants them,
    # because a volume with a DOS shadow of every long name is what Windows gives most users.
    & fsutil.exe 8dot3name set "${letter}:" 0 | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'fsutil 8dot3name set failed' }
    $root = "\\?\${letter}:\"
    $drive = "${letter}:\"

    # The change journal, and then a few operations whose records are known: a file made and written, a file renamed, a
    # file made and deleted at once. The tree below makes the rest, and every record Windows ends up with is read back
    # before the volume is dismounted (`u` lines).
    & fsutil.exe usn createjournal m=4194304 a=1048576 "${letter}:" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'fsutil usn createjournal failed' }
    $scenario = $root + 'journal-scenario'
    $null = [IO.Directory]::CreateDirectory($scenario)
    [IO.File]::WriteAllText("$scenario\made.txt", "made by Windows`r`n", $utf8)
    [IO.File]::WriteAllText("$scenario\before-rename.txt", "to be renamed`r`n", $utf8)
    [IO.File]::Move("$scenario\before-rename.txt", "$scenario\after-rename.txt")
    [IO.File]::WriteAllText("$scenario\deleted-at-once.txt", "short lived`r`n", $utf8)
    [IO.File]::Delete("$scenario\deleted-at-once.txt")

    # ------------------------------------------------------------ the tree
    $manifest = New-Object System.Collections.Generic.List[string]
    $gone = New-Object System.Collections.Generic.List[string]
    $reparse = New-Object System.Collections.Generic.List[object]
    $links = New-Object System.Collections.Generic.List[object]
    $c = $ClusterBytes
    $large = $Scale -eq 'large'

    function Full([string]$Relative) { $root + $Relative.Replace('/', '\') }
    function Make-Dir([string]$Relative) { $null = [IO.Directory]::CreateDirectory((Full $Relative)) }
    function Put([string]$Relative, [byte[]]$Bytes) {
        $path = Full $Relative
        $parent = [IO.Path]::GetDirectoryName($path)
        if (-not [IO.Directory]::Exists($parent)) { $null = [IO.Directory]::CreateDirectory($parent) }
        [IO.File]::WriteAllBytes($path, $Bytes)
    }
    function PutText([string]$Relative, [string]$Text) { Put $Relative ($utf8.GetBytes($Text)) }
    function U([int[]]$CodePoints) { -join ($CodePoints | ForEach-Object { [char]::ConvertFromUtf32($_) }) }
    function Seeded([string]$Relative, [int]$Length) { Put $Relative ([Prng]::Bytes("$Name/$Relative", $Length)) }

    # Sizes around the cluster size, and the two sizes where a file stops being stored in its record.
    PutText 'readme.txt' "Captain Salvage Windows oracle volume $Name`r`nWritten by Windows, read by the app.`r`n"
    Put 'empty.bin' ([byte[]]@())
    Seeded 'one-byte.bin' 1
    Seeded 'resident-700.bin' 700
    Seeded 'nonresident-3000.bin' 3000
    Seeded 'cluster-minus-1.bin' ($c - 1)
    Seeded 'cluster-exact.bin' $c
    Seeded 'cluster-plus-1.bin' ($c + 1)
    Seeded 'ten-clusters-plus-7.bin' (10 * $c + 7)
    Seeded 'ending-in-zeros.bin' 20000
    Put 'zeros-20000.bin' (New-Object byte[] 20000)

    # Nested folders.
    Seeded 'nested/a/b/c/d/e/f/depth-7.txt' 1234
    Seeded 'nested/a/sibling.bin' 5000
    Make-Dir 'empty-folder'
    Make-Dir 'nested/a/b/empty-too'

    # A path longer than the 260 characters Win32 normally allows.
    $chain = (1..14 | ForEach-Object { 'level-{0:00}-' -f $_ + ('d' * 14) }) -join '/'
    Seeded "long-path/$chain/at-the-bottom.bin" 4321

    # Many files in one folder: the folder's index outgrows its record and needs INDX blocks.
    $count = if ($large) { 1200 } else { 400 }
    for ($i = 1; $i -le $count; $i++) {
        $size = if ($i % 5 -eq 0) { 0 } elseif ($i % 5 -eq 1) { 1 + ($i * 13) % 600 } else { 700 + ($i * 977) % 6000 }
        Seeded ('many/file-{0:0000}.dat' -f $i) $size
    }
    # Long names make the index entries big, so the same number of files needs more blocks.
    for ($i = 1; $i -le 150; $i++) {
        Seeded ('many-long/{0:000}-' -f $i + ('n' * 120) + '.txt') (50 + $i)
    }

    # Names.
    $nameList = @(
        @('plain name with spaces.txt', 'plain'),
        @("punctuation !#`$%&'()+,-.;=@[]^_``{}~.txt", 'punct'),
        @('Long Mixed Case File Name.Text', 'mixed'),
        @('.dot-leading', 'dot'),
        @(('caf' + (U 0xE9) + '.txt'), 'latin1'),
        @(((U 0xDC, 0x6E, 0xEF, 0x63, 0xF6, 0x64, 0xE9) + '-' + (U 0xDF, 0x151) + '.txt'), 'latin-ext'),
        @(((U 0x65E5, 0x672C, 0x8A9E) + '.txt'), 'cjk'),
        @(((U 0x1F600) + '-emoji-' + (U 0x1F9F0) + '.txt'), 'emoji'),
        @(((U 0x444, 0x430, 0x439, 0x43B) + '.txt'), 'cyrillic'),
        @(((U 0x3B1, 0x3B2, 0x3B3) + '.txt'), 'greek'),
        @(((U 0x5E9, 0x5DC, 0x5D5, 0x5DD) + '.txt'), 'hebrew'),
        @((('e' + (U 0x301)) + '-decomposed.txt'), 'nfd'),
        @((('L' * 251) + '.txt'), 'name-255'),
        @((((U 0x1F600) * 100) + '.txt'), 'surrogates-200'))
    foreach ($entry in $nameList) {
        Seeded ('names/' + $entry[0]) (100 + $entry[1].Length * 311)
    }
    Seeded ('names/' + (U 0x76EE, 0x5F55) + ' folder/' + (U 0x5185, 0x5BB9) + '.txt') 2222

    # A file written in alternating pieces with another is fragmented in the allocation map. The
    # pieces are flushed to the volume one at a time, so the allocator cannot batch them.
    $piece = if ($large) { 262144 } else { 65536 }
    $pieces = if ($large) { 96 } else { 24 }
    Make-Dir 'fragmented'
    $one = [IO.File]::Open((Full 'fragmented/interleaved-one.bin'), 'Create', 'Write', 'None')
    $two = [IO.File]::Open((Full 'fragmented/interleaved-two.bin'), 'Create', 'Write', 'None')
    try {
        for ($i = 0; $i -lt $pieces; $i++) {
            $a = [Prng]::Bytes("$Name/frag-one/$i", $piece); $one.Write($a, 0, $a.Length); $one.Flush($true)
            $b = [Prng]::Bytes("$Name/frag-two/$i", $piece); $two.Write($b, 0, $b.Length); $two.Flush($true)
        }
    } finally { $one.Dispose(); $two.Dispose() }
    # And one that fills the holes left by deleting every other small file.
    for ($i = 0; $i -lt 40; $i++) { Seeded ("fragmented/holes/h-{0:00}.bin" -f $i) (3 * $c) }
    for ($i = 0; $i -lt 40; $i += 2) { Remove-Item -LiteralPath (Full ("fragmented/holes/h-{0:00}.bin" -f $i)) }
    Seeded 'fragmented/fills-the-holes.bin' (30 * $c)

    # A sparse file: written at the start, the middle and the end of a file far larger than that.
    Make-Dir 'sparse'
    $sparse = Full 'sparse/sparse.bin'
    $sparseLength = if ($large) { 256MB } else { 4MB }
    $stream = [IO.File]::Open($sparse, 'Create', 'ReadWrite', 'None')
    $stream.Dispose()
    & fsutil.exe sparse setflag "${drive}sparse\sparse.bin" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'fsutil sparse setflag failed' }
    $stream = [IO.File]::Open($sparse, 'Open', 'ReadWrite', 'None')
    try {
        $stream.SetLength($sparseLength)
        foreach ($at in 0, ($sparseLength / 2 + 12345), ($sparseLength - 70000)) {
            $data = [Prng]::Bytes("$Name/sparse/$at", 65536 + 17)
            $null = $stream.Seek($at, 'Begin'); $stream.Write($data, 0, $data.Length)
        }
    } finally { $stream.Dispose() }
    # A sparse file with no data at all, and one that is only data.
    $stream = [IO.File]::Open((Full 'sparse/all-hole.bin'), 'Create', 'ReadWrite', 'None'); $stream.Dispose()
    & fsutil.exe sparse setflag "${drive}sparse\all-hole.bin" | Out-Null
    $stream = [IO.File]::Open((Full 'sparse/all-hole.bin'), 'Open', 'ReadWrite', 'None'); $stream.SetLength(10 * $c + 5); $stream.Dispose()

    # Alternate data streams, on files and on a folder, resident and not.
    Seeded 'streams/with-streams.txt' 900
    Seeded 'streams/stream-only.txt' 0
    $streamTargets = @(
        @('streams/with-streams.txt', 'meta', 40),
        @('streams/with-streams.txt', 'big', 25000),
        @('streams/with-streams.txt', ('t' + (U 0xE9) + 'moin'), 77),
        @('streams/stream-only.txt', 'only', 300))
    Make-Dir 'streams/folder-with-stream'
    $streamTargets += , @('streams/folder-with-stream', 'dirdata', 123)
    foreach ($target in $streamTargets) {
        $bytes = [Prng]::Bytes("$Name/stream/$($target[0])/$($target[1])", $target[2])
        $path = (Full $target[0]) + ':' + $target[1]
        $s = [IO.File]::Open($path, 'Create', 'Write', 'None')
        try { $s.Write($bytes, 0, $bytes.Length) } finally { $s.Dispose() }
    }

    # Attributes.
    Seeded 'attributes/hidden.bin' 500
    Seeded 'attributes/system.bin' 500
    Seeded 'attributes/readonly.bin' 500
    [IO.File]::SetAttributes((Full 'attributes/hidden.bin'), 'Hidden')
    [IO.File]::SetAttributes((Full 'attributes/system.bin'), 'System, Hidden')
    [IO.File]::SetAttributes((Full 'attributes/readonly.bin'), 'ReadOnly')

    # NTFS compression, by folder and by file. Text that compresses well, and noise that does not.
    Make-Dir 'compressed'
    & compact.exe /c "${drive}compressed" | Out-Null
    $text = (1..4000 | ForEach-Object { "line $_ of the compressible file, again and again`r`n" }) -join ''
    PutText 'compressed/lznt1-text.txt' $text
    Seeded 'compressed/lznt1-noise.bin' 70000
    PutText 'compressed/lznt1-small.txt' ('tiny ' * 20)
    PutText 'compressed-one-file.txt' $text
    & compact.exe /c "${drive}compressed-one-file.txt" | Out-Null

    # Reparse points and hard links.
    Make-Dir 'links'
    New-Item -ItemType Junction -Path "${drive}links\junction-to-nested" -Value "${drive}nested" | Out-Null
    New-Item -ItemType SymbolicLink -Path "${drive}links\symlink-to-readme" -Value "${drive}readme.txt" | Out-Null
    New-Item -ItemType SymbolicLink -Path "${drive}links\symlink-to-nested" -Value "${drive}nested" | Out-Null
    $reparse.Add(@('junction', 'links/junction-to-nested'))
    $reparse.Add(@('symlink', 'links/symlink-to-readme'))
    $reparse.Add(@('symlink', 'links/symlink-to-nested'))
    Seeded 'links/hard-original.bin' 6000
    & fsutil.exe hardlink create "${drive}links\hard-second-name.bin" "${drive}links\hard-original.bin" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'fsutil hardlink create failed' }
    $links.Add(@('links/hard-original.bin', 'links/hard-second-name.bin'))

    # One big file, so a run of clusters is thousands long.
    if ($large) { Seeded 'big/large-48m.bin' (48MB) }
    else { Seeded 'big/one-megabyte.bin' (1MB) }

    # Files that are deleted last. Their content is hashed first, and nothing is written after.
    Seeded 'gone/small-gone.txt' 150
    Seeded 'gone/big-gone.bin' 90000
    Seeded 'gone/many-clusters-gone.bin' (40 * $c + 1)
    Seeded ('gone/' + (U 0x6D88, 0x3048, 0x305F) + '-gone.bin') 15000
    Seeded 'gone-folder/inside-gone.bin' 33000
    Seeded 'gone-folder/sub/deeper-gone.bin' 12000
    foreach ($relative in 'gone/small-gone.txt', 'gone/big-gone.bin', 'gone/many-clusters-gone.bin',
        ('gone/' + (U 0x6D88, 0x3048, 0x305F) + '-gone.bin'), 'gone-folder/inside-gone.bin', 'gone-folder/sub/deeper-gone.bin') {
        $gone.Add($relative)
    }

    # ----------------------------------------------- what Windows says it wrote
    $windowsOwned = @('System Volume Information', '$RECYCLE.BIN')
    $goneSet = @{}
    foreach ($relative in $gone) { $goneSet[$relative] = $true }
    foreach ($relative in 'gone-folder', 'gone-folder/sub') { $goneSet[$relative] = $true }
    function Relative([string]$FullName) { $FullName.Substring($root.Length).Replace('\', '/') }
    function Hash([string]$Path) { (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant() }

    function Walk([string]$Directory) {
        foreach ($item in (Get-ChildItem -LiteralPath $Directory -Force | Sort-Object Name)) {
            $relative = Relative $item.FullName
            if ($windowsOwned -contains $relative) { $manifest.Add("x`t$relative"); continue }
            $flags = $item.Attributes.ToString()
            $isDirectory = $item.PSIsContainer
            $isReparse = [bool]($item.Attributes -band [IO.FileAttributes]::ReparsePoint)
            if ($isReparse) { continue }   # recorded from what was made, below
            if ($goneSet.ContainsKey($relative)) { continue }   # deleted last, recorded as `g`
            if ($isDirectory) {
                $manifest.Add("d`t$relative")
            } elseif (-not $goneSet.ContainsKey($relative)) {
                $manifest.Add("f`t$($item.Length)`t$(Hash $item.FullName)`t$relative")
            }
            if ($flags -notmatch '^(Archive|Directory|Normal)$' -and -not $goneSet.ContainsKey($relative)) {
                $manifest.Add("a`t$flags`t$relative")
            }
            if (-not $goneSet.ContainsKey($relative)) {
                foreach ($line in [Streams]::Of($item.FullName)) {
                    $name, $length = $line -split "`t"
                    $manifest.Add("s`t$length`t$(Hash ($item.FullName + ':' + $name))`t${relative}:$name")
                }
                if (-not $isDirectory -and $item.Length -gt 0) {
                    # Holes of a sparse file are extents too, with no LCN; only allocated runs count. fsutil
                    # cannot open a path of more than 260 characters, and then it reports nothing: no line.
                    $report = @(& fsutil.exe file queryExtents $item.FullName 2>&1)
                    if ($LASTEXITCODE -eq 0) {
                        $extents = @($report | Where-Object { $_ -match '^\s*VCN:' -and $_ -notmatch 'LCN:\s*0xf{16}\b' }).Count
                        $manifest.Add("e`t$extents`t$relative")
                    }
                }
                $short = [ShortName]::Of($item.FullName)
                if ($short -and $short -ne $item.Name) { $manifest.Add("n`t$short`t$relative") }
            }
            if ($isDirectory) { Walk $item.FullName }
        }
    }
    $manifest.Add("m`tname`t$Name")
    $manifest.Add("m`tcluster_bytes`t$ClusterBytes")
    $manifest.Add("m`tsector_bytes`t$SectorBytes")
    $manifest.Add("m`tvolume_mib`t$SizeMB")
    $manifest.Add("m`tscale`t$Scale")
    $manifest.Add("m`twindows`t$([Environment]::OSVersion.Version)")
    Walk $root
    foreach ($entry in $reparse) { $manifest.Add("l`t$($entry[0])`t$($entry[1])") }
    foreach ($entry in $links) { $manifest.Add("h`t$($entry[0])`t$($entry[1])") }
    $probes = New-Object 'System.Collections.Generic.List[byte[]]'
    foreach ($relative in $gone) {
        $path = Full $relative
        $manifest.Add("g`t$((Get-Item -LiteralPath $path).Length)`t$(Hash $path)`t$relative")
        $all = [IO.File]::ReadAllBytes($path)
        $probes.Add([byte[]]$all[0..31])
    }

    # ------------------------------------------------ delete, then capture
    foreach ($relative in $gone) { Remove-Item -LiteralPath (Full $relative) -Force }
    Remove-Item -LiteralPath (Full 'gone-folder') -Recurse -Force
    # A deleted folder takes its name with it: the folder is not in the manifest, `gone/` is.
    $manifest.Add("m`tdeleted_folders`tgone-folder")

    $chk = (& chkdsk.exe "${letter}:" 2>&1 | Out-String) -split "\r?\n|\r" | Where-Object { $_ -notmatch '^Progress:' }
    $chk = $chk -join "`n"
    if ($chk -notmatch 'Windows has scanned the file system and found no problems') { throw "chkdsk found a problem:`n$chk" }
    $ntfsinfo = & fsutil.exe fsinfo ntfsinfo "${letter}:" | Out-String
    $dir = & cmd.exe /c "dir /x /a `"${drive}names`"" | Out-String
    Write-VolumeCache -DriveLetter $letter
    $partitionNow = Get-Partition -DiskNumber $number | Where-Object { $_.DriveLetter -eq $letter }
    $diskNow = Get-Disk -Number $number
    $info = @(
        "name: $Name", "windows: $([Environment]::OSVersion.Version) $((Get-CimInstance Win32_OperatingSystem).Caption)",
        "powershell: $($PSVersionTable.PSVersion)", "disk tool: $diskTool", "partition style: $PartitionStyle",
        "logical sector bytes: $($diskNow.LogicalSectorSize)", "physical sector bytes: $($diskNow.PhysicalSectorSize)",
        "partition offset: $($partitionNow.Offset)", "partition bytes: $($partitionNow.Size)",
        "formatter: Format-Volume -FileSystem NTFS -AllocationUnitSize $ClusterBytes", '',
        '--- fsutil fsinfo ntfsinfo ---', $ntfsinfo, '--- chkdsk (read only) ---', $chk, '--- dir /x names ---', $dir)
    [IO.File]::WriteAllText((Join-Path $Out "$Name.info.txt"), ($info -join "`r`n"), $utf8)

    # Every record of the change journal, as Windows reads it, before the volume goes away.
    foreach ($line in [Usn]::Read("\\.\${letter}:")) { $manifest.Add("u`t$line") }
    & fsutil.exe volume dismount "${letter}:" | Out-Null
    $kept = [RawDisk]::Dump("\\.\PhysicalDrive$number", [long]$partitionNow.Offset, [long]$partitionNow.Size,
        (Join-Path $Out "$Name.img.gz"), $probes.ToArray())
    # Whether the first 32 bytes of each deleted file are still anywhere in the volume. Windows scrubs
    # a small file's data with its record, and then nothing of the file can be found.
    for ($i = 0; $i -lt $gone.Count; $i++) { $manifest.Add("p`t$([int]$kept[$i])`t$($gone[$i])") }
    [IO.File]::WriteAllText((Join-Path $Out "$Name.manifest.tsv"), (($manifest -join "`n") + "`n"), $utf8)
    Write-Host "make-volume: $Name captured ($($manifest.Count) manifest lines, $([long]$partitionNow.Size) bytes of volume)"
} finally {
    if ($attached) {
        try { Dismount-DiskImage -ImagePath $disk -ErrorAction Stop | Out-Null } catch { Write-Warning "dismount: $_" }
    }
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
