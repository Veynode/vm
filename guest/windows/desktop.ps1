# Desktop and accessibility methods of a Windows guest, for the daemon's MCP
# tools (vm_desktop_state, vm_accessibility_tree, ...). Runs in the logged-on
# user's desktop session (session.ps1 starts it) with Windows PowerShell 5.1:
# windows through Win32, the accessibility tree through UI Automation.
# Nothing is installed. Positions are physical screen pixels, the same pixels
# as the console screenshot.
#
# Argument: a directory with request.json ({"method":..., "arguments":{...}});
# writes out.json (the result or {"error": ...}) and then done.
param([string]$Dir)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$deadline = (Get-Date).AddSeconds(8)
$maxVisited = 6000
$maxChildren = 500
$maxText = 2000

Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class VW {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsZoomed(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern IntPtr GetWindow(IntPtr h, uint cmd);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll", EntryPoint = "GetWindowLongPtrW")] public static extern IntPtr GetWindowLongPtr(IntPtr h, int i);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int size);
    [DllImport("dwmapi.dll")] static extern int DwmGetWindowAttribute(IntPtr h, int a, out int v, int size);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern IntPtr GetKeyboardLayout(uint thread);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [DllImport("user32.dll")] static extern bool SystemParametersInfo(uint a, uint b, out RECT r, uint c);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr c);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] static extern bool AttachThreadInput(uint a, uint b, bool attach);
    [DllImport("kernel32.dll")] static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int cx, int cy, uint flags);

    public static List<IntPtr> TopLevel() {
        var all = new List<IntPtr>();
        EnumWindows((h, l) => { all.Add(h); return true; }, IntPtr.Zero);
        return all;
    }
    public static string Title(IntPtr h) { var s = new StringBuilder(512); GetWindowText(h, s, s.Capacity); return s.ToString(); }
    public static string Class(IntPtr h) { var s = new StringBuilder(256); GetClassName(h, s, s.Capacity); return s.ToString(); }
    public static bool Cloaked(IntPtr h) { int v; return DwmGetWindowAttribute(h, 14, out v, 4) == 0 && v != 0; }
    // The visible frame: GetWindowRect includes the invisible resize borders.
    public static RECT Frame(IntPtr h) { RECT r; if (DwmGetWindowAttribute(h, 9, out r, 16) != 0) GetWindowRect(h, out r); return r; }
    public static RECT Outer(IntPtr h) { RECT r; GetWindowRect(h, out r); return r; }
    public static RECT WorkArea() { RECT r; SystemParametersInfo(0x30, 0, out r, 0); return r; }
    public static uint Pid(IntPtr h) { uint pid; GetWindowThreadProcessId(h, out pid); return pid; }
    // Windows lets only the foreground process change the foreground window;
    // sharing its input state for the call lifts that.
    public static bool Activate(IntPtr h) {
        if (IsIconic(h)) ShowWindow(h, 9);
        uint dummy;
        uint fg = GetWindowThreadProcessId(GetForegroundWindow(), out dummy), me = GetCurrentThreadId();
        bool attached = fg != 0 && fg != me && AttachThreadInput(me, fg, true);
        BringWindowToTop(h);
        bool done = SetForegroundWindow(h);
        if (attached) AttachThreadInput(me, fg, false);
        return done;
    }
}
'@
# Per-monitor DPI aware before any window or UIA call: physical pixels.
[void][VW]::SetProcessDpiAwarenessContext([IntPtr](-4))
Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$AE = [Windows.Automation.AutomationElement]

function Box($left, $top, $width, $height) {
    [ordered]@{ x = [int]$left; y = [int]$top; width = [int]$width; height = [int]$height
        center_x = [int]($left + $width / 2); center_y = [int]($top + $height / 2) }
}

function Words([string]$name) {
    # ControlType.MenuItem -> menu item
    (($name -replace '^ControlType\.', '') -creplace '([a-z])([A-Z])', '$1 $2').ToLower()
}

$processNames = @{}
function ProcessName([uint32]$id) {
    if (-not $processNames.ContainsKey($id)) {
        $p = Get-Process -Id $id -ErrorAction SilentlyContinue
        $processNames[$id] = if ($p) { $p.ProcessName } else { '' }
    }
    $processNames[$id]
}

# Top-level windows a user would see (the Alt+Tab rule): visible, not
# cloaked (other virtual desktops, suspended Store apps), not tool windows,
# unowned or marked as app windows. Top-most first.
function UserWindows {
    foreach ($h in [VW]::TopLevel()) {
        if (-not [VW]::IsWindowVisible($h) -or [VW]::Cloaked($h)) { continue }
        $ex = [VW]::GetWindowLongPtr($h, -20).ToInt64()
        $tool = ($ex -band 0x80) -ne 0; $app = ($ex -band 0x40000) -ne 0
        $owner = [VW]::GetWindow($h, 4) -ne [IntPtr]::Zero
        if ($tool -or ($owner -and -not $app)) { continue }
        if (-not [VW]::Title($h) -and [VW]::Class($h) -ne 'Shell_TrayWnd') { continue }
        $h
    }
}

function Describe([IntPtr]$h, [IntPtr]$front) {
    $class = [VW]::Class($h); $id = [VW]::Pid($h)
    $minimized = [VW]::IsIconic($h); $active = $h -eq $front
    $states = @()
    if ($active) { $states += 'focused' }
    if ($minimized) { $states += 'minimized' }
    if ([VW]::IsZoomed($h)) { $states += 'maximized' }
    $ex = [VW]::GetWindowLongPtr($h, -20).ToInt64()
    if ($ex -band 0x8) { $states += 'topmost' }
    $r = [VW]::Frame($h)
    $type = switch ($class) { '#32770' { 'dialog' } 'Shell_TrayWnd' { 'taskbar' } default { 'window' } }
    $out = [ordered]@{ id = $h.ToInt64(); title = [VW]::Title($h); app = (ProcessName $id); pid = [int]$id
        class = $class; type = $type; states = $states; active = $active; minimized = $minimized }
    if (-not $minimized) {
        $out.x = $r.Left; $out.y = $r.Top; $out.width = $r.Right - $r.Left; $out.height = $r.Bottom - $r.Top
    }
    $out
}

function Window([long]$id) {
    $h = [IntPtr]$id
    if (-not [VW]::IsWindow($h) -or (UserWindows) -notcontains $h) { throw "no window $id (get ids from vm_desktop_state)" }
    $h
}

function DesktopState {
    $front = [VW]::GetForegroundWindow()
    $work = [VW]::WorkArea()
    $cursor = New-Object VW+POINT
    [void][VW]::GetCursorPos([ref]$cursor)
    $ignored = [uint32]0
    $thread = [VW]::GetWindowThreadProcessId($front, [ref]$ignored)
    $hkl = [VW]::GetKeyboardLayout($thread).ToInt64()
    $keyboard = [ordered]@{ layout = '{0:x8}' -f ($hkl -band 0xffffffffL) }
    try { $keyboard.name = ([Globalization.CultureInfo]::GetCultureInfo([int]($hkl -band 0xffff))).DisplayName } catch {}
    $windows = @(UserWindows | ForEach-Object { Describe $_ $front })
    $result = [ordered]@{
        coordinate_space = 'screen-pixels'
        screen = [ordered]@{ width = [VW]::GetSystemMetrics(0); height = [VW]::GetSystemMetrics(1) }
        work_area = [ordered]@{ x = $work.Left; y = $work.Top; width = $work.Right - $work.Left; height = $work.Bottom - $work.Top }
        cursor = [ordered]@{ x = $cursor.X; y = $cursor.Y }
        keyboard = $keyboard
        windows = $windows
    }
    if ($front -ne [IntPtr]::Zero) {
        $id = [VW]::Pid($front)
        $result.focused_app = [ordered]@{ pid = [int]$id; name = (ProcessName $id); window = $front.ToInt64(); title = [VW]::Title($front) }
    }
    $result
}

# Start menu entries: Store apps (AUMID) and shortcuts. ids are names in
# the tool's id alphabet; launching resolves them again.
function StartApps {
    $seen = @{}
    foreach ($app in (Get-StartApps | Sort-Object Name)) {
        $slug = (($app.Name.ToLower() -replace '[^a-z0-9._+-]+', '-') -replace '^[^a-z0-9]+', '').Trim('-')
        if (-not $slug) { $slug = 'app' }
        if ($slug.Length -gt 190) { $slug = $slug.Substring(0, 190) }
        $id = $slug; $n = 2
        while ($seen.ContainsKey($id)) { $id = "$slug-$n"; $n++ }
        $seen[$id] = $true
        [pscustomobject]@{ id = $id; name = $app.Name; app_id = $app.AppID }
    }
}

function DesktopApps {
    $running = @{}
    foreach ($p in Get-Process) { if ($p.Path) { $running[$p.Path.ToLower()] = $true } }
    $apps = foreach ($app in StartApps) {
        $out = [ordered]@{ id = $app.id; name = $app.name; app_id = $app.app_id }
        $out.kind = if ($app.app_id -match '!') { 'store' } else { 'desktop' }
        $path = $app.app_id -replace '^\{6D809377-6AF0-444B-8957-A3773F02200E\}', $env:ProgramFiles -replace '^\{7C5A40EF-A0FB-4BFC-874A-C0F2E0B9FA8E\}', ${env:ProgramFiles(x86)} -replace '^\{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7\}', "$env:SystemRoot\System32" -replace '^\{F38BF404-1D43-42F2-9305-67DE0B28FC23\}', $env:SystemRoot
        if ($path -match '^[A-Za-z]:\\.*\.exe$') { $out.path = $path; $out.running = $running.ContainsKey($path.ToLower()) }
        $out
    }
    [ordered]@{ apps = @($apps) }
}

function WindowAction($a) {
    $h = Window $a.window
    switch ($a.action) {
        'activate' { $done = [VW]::Activate($h) }
        'close' { $done = [VW]::PostMessage($h, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) }
        'minimize' { $done = [VW]::ShowWindow($h, 6) }
        'maximize' { $done = [VW]::ShowWindow($h, 3) }
        'unmaximize' { $done = [VW]::ShowWindow($h, 9) }
        { $_ -in 'fullscreen', 'unfullscreen' } { throw 'unsupported: Windows has no system full-screen state for a window; use maximize, or the application''s own key (often F11) with vm_input_key' }
        'move_resize' {
            if ([VW]::IsIconic($h) -or [VW]::IsZoomed($h)) { [void][VW]::ShowWindow($h, 9) }
            # x, y, width and height describe the visible frame; the window
            # rectangle adds invisible borders around it.
            $frame = [VW]::Frame($h); $outer = [VW]::Outer($h)
            $left = $frame.Left - $outer.Left; $top = $frame.Top - $outer.Top
            $right = $outer.Right - $frame.Right; $bottom = $outer.Bottom - $frame.Bottom
            $x = if ($null -ne $a.x) { $a.x } else { $frame.Left }
            $y = if ($null -ne $a.y) { $a.y } else { $frame.Top }
            $w = if ($null -ne $a.width) { $a.width } else { $frame.Right - $frame.Left }
            $hh = if ($null -ne $a.height) { $a.height } else { $frame.Bottom - $frame.Top }
            # SWP_NOZORDER | SWP_NOACTIVATE
            $done = [VW]::SetWindowPos($h, [IntPtr]::Zero, $x - $left, $y - $top, $w + $left + $right, $hh + $top + $bottom, 0x14)
        }
    }
    Start-Sleep -Milliseconds 150
    $after = $null
    if ([VW]::IsWindow($h) -and (UserWindows) -contains $h) { $after = Describe $h ([VW]::GetForegroundWindow()) }
    [ordered]@{ requested = $true; done = [bool]$done; window = $h.ToInt64(); action = $a.action; window_after = $after }
}

function AppLaunch($a) {
    $app = StartApps | Where-Object id -eq $a.app | Select-Object -First 1
    if (-not $app) { throw "no application $($a.app) (get ids from vm_desktop_apps)" }
    $before = @(UserWindows)
    Start-Process explorer.exe "shell:AppsFolder\$($app.app_id)"
    $result = [ordered]@{ launched = $true; app = $app.id; name = $app.name; window = $null }
    $until = (Get-Date).AddSeconds(5)
    while ((Get-Date) -lt $until) {
        Start-Sleep -Milliseconds 100
        $new = @(UserWindows | Where-Object { $before -notcontains $_ })
        if ($new) {
            [void][VW]::Activate($new[0])
            Start-Sleep -Milliseconds 100
            $result.pid = [int][VW]::Pid($new[0])
            $result.window = Describe $new[0] ([VW]::GetForegroundWindow())
            return $result
        }
    }
    $result.note = 'no new window within 5 s; it may still appear (use screen_wait or desktop_state), an existing window may have been reused, or the application runs without one'
    $result
}

# --- UI Automation ---
$cache = New-Object Windows.Automation.CacheRequest
$cache.TreeFilter = [Windows.Automation.Automation]::ControlViewCondition
$P = @{
    toggle = [Windows.Automation.TogglePatternIdentifiers]::ToggleStateProperty
    expand = [Windows.Automation.ExpandCollapsePatternIdentifiers]::ExpandCollapseStateProperty
    selected = [Windows.Automation.SelectionItemPatternIdentifiers]::IsSelectedProperty
    value = [Windows.Automation.ValuePatternIdentifiers]::ValueProperty
    readonly = [Windows.Automation.ValuePatternIdentifiers]::IsReadOnlyProperty
    range = [Windows.Automation.RangeValuePatternIdentifiers]::ValueProperty
}
foreach ($property in $AE::NameProperty, $AE::ControlTypeProperty, $AE::BoundingRectangleProperty, $AE::IsOffscreenProperty,
    $AE::IsEnabledProperty, $AE::HasKeyboardFocusProperty, $AE::IsKeyboardFocusableProperty, $AE::AutomationIdProperty,
    $AE::ClassNameProperty, $AE::ProcessIdProperty, $AE::HelpTextProperty, $AE::IsInvokePatternAvailableProperty,
    $AE::IsTogglePatternAvailableProperty, $AE::IsExpandCollapsePatternAvailableProperty,
    $AE::IsSelectionItemPatternAvailableProperty, $AE::IsValuePatternAvailableProperty,
    $AE::IsRangeValuePatternAvailableProperty, $AE::IsTextPatternAvailableProperty, $P.toggle, $P.expand, $P.selected,
    $P.value, $P.readonly, $P.range) {
    $cache.Add($property)
}
$cache.Push()
$notSupported = $AE::NotSupported

function Cached($e, $property) {
    $v = $e.GetCachedPropertyValue($property, $true)
    # Not -eq: PowerShell would convert the sentinel to the left side's type.
    if ([object]::ReferenceEquals($v, $notSupported)) { $null } else { $v }
}

function Kids($e) {
    try { $e.FindAll([Windows.Automation.TreeScope]::Children, [Windows.Automation.Automation]::ControlViewCondition) } catch { @() }
}

function LastLines([string]$text) {
    $lines = $text -split "`r?`n"
    if ($lines.Count -gt 40) { $text = ($lines[-40..-1]) -join "`n" }
    if ($text.Length -gt $maxText) { $text = $text.Substring($text.Length - $maxText) }
    $text
}

function Key($e) {
    $key = Cached $e $AE::AutomationIdProperty
    if (-not $key) { $key = Cached $e $AE::NameProperty }
    if ($key.Length -gt 200) { $key = $key.Substring(0, 200) }
    "$key"
}

function Actions($e) {
    $names = @()
    if (Cached $e $AE::IsInvokePatternAvailableProperty) { $names += 'invoke' }
    if (Cached $e $AE::IsTogglePatternAvailableProperty) { $names += 'toggle' }
    if (Cached $e $AE::IsSelectionItemPatternAvailableProperty) { $names += 'select' }
    if (Cached $e $AE::IsExpandCollapsePatternAvailableProperty) {
        $names += if ((Cached $e $P.expand) -eq [Windows.Automation.ExpandCollapseState]::Expanded) { 'collapse' } else { 'expand' }
    }
    $names
}

function Detail($e, [long]$hwnd, [string]$path, [int]$depth) {
    $role = Words (Cached $e $AE::ControlTypeProperty).ProgrammaticName
    $out = [ordered]@{ depth = $depth; role = $role }
    $name = Cached $e $AE::NameProperty
    if ($name) { $out.name = $name }
    $help = Cached $e $AE::HelpTextProperty
    if ($help) { $out.description = $help }
    $value = Cached $e $P.value
    if ($value -and $value -ne $name) { $out.text = LastLines $value }
    elseif ((Cached $e $AE::IsTextPatternAvailableProperty) -and $role -in 'document', 'edit') {
        try {
            $range = $e.GetCurrentPattern([Windows.Automation.TextPattern]::Pattern).DocumentRange
            $text = $range.GetText(65536)
            if ($text) { $out.text = LastLines $text }
        } catch {}
    }
    $number = Cached $e $P.range
    if ($null -ne $number) { $out.value = [double]$number }
    $id = Cached $e $AE::AutomationIdProperty
    if ($id) { $out.identifier = $id }
    $class = Cached $e $AE::ClassNameProperty
    if ($class) { $out.class = $class }
    $states = @()
    if (Cached $e $AE::HasKeyboardFocusProperty) { $states += 'focused' }
    if (-not (Cached $e $AE::IsEnabledProperty)) { $states += 'disabled' }
    switch ([string](Cached $e $P.toggle)) { 'On' { $states += 'checked' } 'Indeterminate' { $states += 'mixed' } }
    switch ([string](Cached $e $P.expand)) { 'Expanded' { $states += 'expanded' } 'Collapsed' { $states += 'collapsed' } }
    if (Cached $e $P.selected) { $states += 'selected' }
    if ((Cached $e $AE::IsValuePatternAvailableProperty) -and (Cached $e $P.readonly)) { $states += 'read only' }
    $offscreen = [bool](Cached $e $AE::IsOffscreenProperty)
    if ($offscreen) { $states += 'offscreen' }
    if ($states) { $out.states = $states }
    $actions = @(Actions $e)
    if ($actions) { $out.actions = $actions }
    $r = Cached $e $AE::BoundingRectangleProperty
    if ($r -and -not $r.IsEmpty -and $r.Width -gt 0 -and $r.Height -gt 0 -and -not [double]::IsInfinity($r.X)) {
        $out.box = Box $r.X $r.Y $r.Width $r.Height
    }
    if ($depth -eq 0) {
        $pid_ = [uint32](Cached $e $AE::ProcessIdProperty)
        $out.app = ProcessName $pid_
        $out.pid = [int]$pid_
        $out.window = $hwnd
    }
    $out.ref = "$hwnd|$path|$role|$(Key $e)"
    [pscustomobject]@{ out = $out; offscreen = $offscreen }
}

function Roots($filter) {
    $front = [VW]::GetForegroundWindow()
    $windows = @(UserWindows)
    $windows = @($windows | Where-Object { $_ -eq $front }) + @($windows | Where-Object { $_ -ne $front })
    foreach ($h in $windows) {
        if ($filter) {
            $id = [VW]::Pid($h)
            if ("$id" -ne $filter -and (ProcessName $id) -notlike "*$filter*" -and [VW]::Title($h) -notlike "*$filter*") { continue }
        }
        try { [pscustomobject]@{ hwnd = $h.ToInt64(); element = $AE::FromHandle($h).GetUpdatedCache($cache) } } catch {}
    }
}

function Matches($out, $name, $role) {
    if ($role -and $out.role -ne $role.ToLower()) { return $false }
    if ($name) {
        $hay = @($out.name, $out.text, $out.description) | Where-Object { $_ }
        if (-not ($hay | Where-Object { $_.IndexOf($name, [StringComparison]::OrdinalIgnoreCase) -ge 0 })) { return $false }
    }
    $true
}

function AccessibilityTree($a) {
    $maxDepth = if ($a.max_depth) { [int]$a.max_depth } else { 25 }
    $maxNodes = if ($a.max_nodes) { [int]$a.max_nodes } else { 400 }
    $includeHidden = [bool]$a.include_hidden
    $name = $a.name; $role = $a.role
    $find = $name -or $role
    $script:visited = 0; $script:truncated = $null
    $nodes = New-Object Collections.ArrayList
    function Walk($e, [long]$hwnd, [string]$path, [int]$depth, $context) {
        if ($script:truncated) { return }
        if ($script:visited -ge $maxVisited) { $script:truncated = 'visited'; return }
        if ((Get-Date) -gt $deadline) { $script:truncated = 'deadline'; return }
        $script:visited++
        $d = Detail $e $hwnd $path $depth
        if ($d.offscreen -and -not $includeHidden -and $depth -gt 0) { return }
        $kids = @()
        if ($depth -lt $maxDepth -or -not $find) { $kids = @(Kids $e) }
        if ($depth -ge $maxDepth -and $kids.Count) { $d.out.unexpanded_children = $kids.Count }
        if (-not $find -or (Matches $d.out $name $role)) {
            if ($nodes.Count -ge $maxNodes) { $script:truncated = 'max_nodes'; return }
            if ($find) { $d.out.context = $context -join ' > ' }
            [void]$nodes.Add($d.out)
        }
        if ($depth -ge $maxDepth) { return }
        $next = $context
        if ($d.out.name -and ($depth -eq 0 -or $context.Count -lt 2)) { $next = @($context) + $d.out.name }
        if ($kids.Count -ge $maxChildren) { $script:truncated = 'children' }
        $i = 0
        foreach ($kid in $kids) {
            if ($i -ge $maxChildren) { break }
            $child = if ($path) { "$path.$i" } else { "$i" }
            Walk $kid $hwnd $child ($depth + 1) $next
            $i++
        }
    }
    foreach ($root in Roots $a.app) {
        if ($root.element) { Walk $root.element $root.hwnd '' 0 @() }
    }
    $result = [ordered]@{ nodes = @($nodes); count = $nodes.Count; visited = $script:visited; coordinate_space = 'screen-pixels' }
    if ($script:truncated) { $result.truncated = $script:truncated }
    $result
}

# A ref is window|child path|role|key. Applications add and remove elements,
# which shifts paths: when the element at the path has another role or key,
# use the window's one element with the ref's role and key.
function Resolve([string]$ref) {
    $parts = $ref.Split('|', 4)
    if ($parts.Count -ne 4 -or $parts[0] -notmatch '^\d+$') { throw 'invalid ref (use a ref from vm_accessibility_tree)' }
    $h = [IntPtr][long]$parts[0]
    if (-not [VW]::IsWindow($h)) { throw 'gone: the element''s window no longer exists' }
    $e = $AE::FromHandle($h).GetUpdatedCache($cache)
    $path = ''
    if ($parts[1]) {
        foreach ($index in $parts[1].Split('.')) {
            $kids = @(Kids $e)
            if ([int]$index -ge $kids.Count) { $e = $null; break }
            $e = $kids[[int]$index]
        }
        $path = $parts[1]
    }
    $role = $parts[2]; $key = $parts[3]
    if ($e -and (Words (Cached $e $AE::ControlTypeProperty).ProgrammaticName) -eq $role -and (Key $e) -eq $key) {
        return [pscustomobject]@{ element = $e; hwnd = $h.ToInt64(); path = $path }
    }
    # Search by role and key, keeping the path for the new ref.
    $found = New-Object Collections.ArrayList
    function Search($node, [string]$at) {
        $i = 0
        foreach ($kid in @(Kids $node)) {
            $here = if ($at) { "$at.$i" } else { "$i" }
            if ((Words (Cached $kid $AE::ControlTypeProperty).ProgrammaticName) -eq $role -and (Key $kid) -eq $key) {
                [void]$found.Add([pscustomobject]@{ element = $kid; hwnd = $h.ToInt64(); path = $here })
            }
            if ($found.Count -gt 1 -or (Get-Date) -gt $deadline) { return }
            Search $kid $here
            $i++
        }
    }
    Search ($AE::FromHandle($h).GetUpdatedCache($cache)) ''
    if ($found.Count -ne 1) { throw "gone: no single element with role $role and key '$key' in that window" }
    $found[0]
}

function AccessibilityAction($a) {
    $target = Resolve $a.ref
    $e = $target.element
    $result = [ordered]@{ ref = (Detail $e $target.hwnd $target.path 1).out.ref; action = $a.action }
    switch ($a.action) {
        'do' {
            $available = @(Actions $e)
            $chosen = if ($a.action_name) {
                if ($available -notcontains $a.action_name) { throw "no action $($a.action_name); the element has: $($available -join ', ')" }
                $a.action_name
            } else { $available | Select-Object -First 1 }
            if (-not $chosen) { throw 'the element has no action (invoke, toggle, select, expand or collapse); use focus, or click its box' }
            switch ($chosen) {
                'invoke' { $e.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke() }
                'toggle' { $e.GetCurrentPattern([Windows.Automation.TogglePattern]::Pattern).Toggle() }
                'select' { $e.GetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern).Select() }
                'expand' { $e.GetCurrentPattern([Windows.Automation.ExpandCollapsePattern]::Pattern).Expand() }
                'collapse' { $e.GetCurrentPattern([Windows.Automation.ExpandCollapsePattern]::Pattern).Collapse() }
            }
            $result.action_name = $chosen
        }
        'focus' { [void][VW]::Activate([IntPtr]$target.hwnd); $e.SetFocus() }
        'set_text' {
            if (-not (Cached $e $AE::IsValuePatternAvailableProperty)) { throw 'the element takes no text (no UI Automation value pattern)' }
            $e.GetCurrentPattern([Windows.Automation.ValuePattern]::Pattern).SetValue([string]$a.text)
        }
        'set_value' {
            if (-not (Cached $e $AE::IsRangeValuePatternAvailableProperty)) { throw 'the element takes no number (no UI Automation range value pattern)' }
            $e.GetCurrentPattern([Windows.Automation.RangeValuePattern]::Pattern).SetValue([double]$a.value)
        }
    }
    $result.done = $true
    Start-Sleep -Milliseconds 100
    try { $result.element_after = (Detail $e.GetUpdatedCache($cache) $target.hwnd $target.path 1).out } catch { $result.element_after = $null }
    $result
}

$utf8 = New-Object Text.UTF8Encoding $false
try {
    $request = [IO.File]::ReadAllText("$Dir\request.json") | ConvertFrom-Json
    $a = $request.arguments
    $output = switch ($request.method) {
        'desktop_state' { DesktopState }
        'desktop_apps' { DesktopApps }
        'window_action' { WindowAction $a }
        'app_launch' { AppLaunch $a }
        'accessibility_tree' { AccessibilityTree $a }
        'accessibility_action' { AccessibilityAction $a }
        default { throw "unknown method $($request.method)" }
    }
    $json = ConvertTo-Json -Depth 8 -Compress $output
} catch {
    $json = ConvertTo-Json -Compress @{ error = "$($_.Exception.Message)" }
}
[IO.File]::WriteAllText("$Dir\out.json", $json, $utf8)
[IO.File]::WriteAllText("$Dir\done", '', $utf8)
