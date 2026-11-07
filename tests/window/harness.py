"""Drives a hidden copy of the app with a throwaway HOME, for window tests. Never the real disk.

The copy may only write recovered files under its HOME, the temp folder, or this folder
(SALVAGE_TEST_COPY_ROOT), so a copy started by a test can never reach a real drive.

The window's browser storage is shared with the user's own copy whatever HOME says, so the keys
the app keeps there are saved first and put back at the end.
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.request

SCRATCH = os.path.dirname(os.path.abspath(__file__))
STORAGE_KEYS = ('lens-column-widths', 'lens-canvas')
results = []
state = {}


def _timeout(*_):
    raise SystemExit('test timed out')


def check(name, got, want):
    results.append((name, got == want))
    print(('PASS ' if got == want else 'FAIL ') + f'{name}: expected {want!r}, got {got!r}', flush=True)


def call(name, /, **arguments):
    info = json.load(open(state['cfg']))
    body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': name, 'arguments': arguments}}).encode()
    headers = {'Content-Type': 'application/json', 'Authorization': 'Bearer ' + info['token']}
    with urllib.request.urlopen(urllib.request.Request(info['url'], body, headers), timeout=120) as response:
        result = json.loads(response.read().decode())['result']
    text = result['content'][0]['text']
    return {'error': text} if result.get('isError') else json.loads(text)


def js(code):
    call('ui_eval', code=code)


def probe(expression, tries=60):
    """Evaluates an expression in the window and reads its value back through set_selection."""
    marker = repr(time.time())
    js("(async()=>{let v;try{v=await (async()=>(%s))()}catch(e){v={jsError:String(e)}}"
       "await window.__TAURI_INTERNALS__.invoke('engine_call',{tool:'set_selection',args:{selection:{probe:{marker:%s,value:v}}}})})()"
       % (expression, json.dumps(marker)))
    for _ in range(tries):
        time.sleep(0.5)
        selection = call('get_selection')
        found = ((selection or {}).get('selection') or {}).get('probe') if isinstance(selection, dict) else None
        if found and found.get('marker') == marker:
            return found.get('value')
    raise SystemExit('the window never answered: ' + expression[:80])


def maybe(expression):
    try:
        return probe(expression, tries=10)
    except SystemExit:
        return None


def wait_for(expression, seconds=60):
    for _ in range(seconds):
        if maybe(expression):
            return True
        time.sleep(1)
    return False


def renamed_copy(binary):
    """Runs a copy of the bundle under its own identifier. A network filter's rule for the real
    app then never holds the test's local ports, and the copy's browser storage is its own."""
    import shutil
    bundle = os.path.abspath(binary).split('/Contents/MacOS/')[0]
    target = os.path.join(SCRATCH, 'testapp', 'Salvage Test.app')
    shutil.rmtree(os.path.dirname(target), ignore_errors=True)
    os.makedirs(os.path.dirname(target))
    subprocess.run(['cp', '-R', bundle, target], check=True)
    plist = os.path.join(target, 'Contents', 'Info.plist')
    subprocess.run(['/usr/libexec/PlistBuddy', '-c', 'Set :CFBundleIdentifier dev.antimatterstudios.captainsalvage.windowtest.r%d' % int(time.time()), '-c', 'Set :CFBundleName Salvage Test', plist], check=True)
    subprocess.run(['codesign', '--force', '-s', '-', '--deep', target], check=True, capture_output=True)
    return os.path.join(target, 'Contents', 'MacOS', os.path.basename(binary))


def start(binary, seconds=600):
    signal.signal(signal.SIGALRM, _timeout)
    signal.alarm(seconds)
    home = os.environ.get('SALVAGE_TEST_HOME') or tempfile.mkdtemp(dir=SCRATCH, prefix='fakehome.')
    state['home'] = home
    state['cfg'] = os.path.join(home, 'Library/Application Support/Captain Salvage/mcp.json')
    env = {'HOME': home, 'USER': os.environ.get('USER', ''), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'SALVAGE_TEST_UI': '1', 'SALVAGE_TEST_HIDDEN': '1', 'SALVAGE_TEST_COPY_ROOT': SCRATCH, **{k: v for k, v in os.environ.items() if k.startswith('SALVAGE_AGENT_')}}
    program = binary if os.environ.get('SALVAGE_TEST_KEEP_ID') else renamed_copy(binary)
    # The fake home has no global preferences, so App Nap is turned off here. SALVAGE_TEST_NAP
    # leaves it on, for reproducing a nap problem.
    nap = [] if os.environ.get('SALVAGE_TEST_NAP') else ['-NSAppSleepDisabled', 'YES']
    state['child'] = subprocess.Popen([program] + nap, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(90):
        time.sleep(1)
        if os.path.exists(state['cfg']):
            break
    else:
        raise SystemExit('the test copy never published its address')
    if not wait_for("!!document.querySelector('.start-image input')"):
        raise SystemExit('the start screen never appeared')


def open_image(path):
    js("(() => {const i=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));setTimeout(()=>i.form.requestSubmit(),300);})()" % json.dumps(path))
    if not wait_for("!!document.querySelector('.safety-chip')"):
        raise SystemExit('the image never opened in the window')


def scan_and_reopen(path):
    open_image(path)
    call('start_scan')
    for _ in range(120):
        time.sleep(1)
        if (call('status').get('scan') or {}).get('status') == 'complete':
            break
    js("location.reload()")
    time.sleep(8)
    if not wait_for("!!document.querySelector('.start-image input')"):
        raise SystemExit('the start screen never came back')
    open_image(path)
    time.sleep(2)


def finish():
    import shutil, traceback
    error = sys.exc_info()[1]
    if error is not None:
        print('STOPPED by an error:', flush=True)
        traceback.print_exception(error)
        results.append(('the test ran to the end', False))
    child = state.get('child')
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
    if state.get('home') and not os.environ.get('SALVAGE_TEST_HOME'):
        shutil.rmtree(state['home'], ignore_errors=True)
    passed = sum(1 for _, ok in results if ok)
    print('%d passed, %d failed' % (passed, len(results) - passed), flush=True)
