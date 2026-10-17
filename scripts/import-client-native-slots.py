import sys, json, hashlib, shutil, re
from pathlib import Path
from elftools.elf.elffile import ELFFile
source = Path(sys.argv[1]).resolve()
root = Path(sys.argv[2]).resolve()
dest = root/'out/orcad-prebuilds'
sealed = json.loads((source/'orcad-template.json').read_text())
manifest = {'schemaVersion':2,'module':'node-pty','version':'1.1.0','napi':8,'nodeHeaders':None,'importedFrom':'stablyai/orca v1.4.224 official Windows installer; native bytes unchanged','slots':{}}
for slot, record in sealed['targets'].items():
    platform, arch = slot.split('-')[:2]
    libc = ('musl' if slot.endswith('musl') else 'glibc') if platform=='linux' else 'none'
    files, glibc = {}, None
    for relative, expected in record['files'].items():
        prefix = 'node_modules/node-pty/build/Release/'
        if relative.startswith(prefix): name = relative[len(prefix):]
        elif slot.endswith('glibc217') and relative=='node_modules/@parcel/watcher/watcher.node': name='parcel-watcher/watcher.node'
        else: continue
        original = source/'targets'/slot/relative
        data = original.read_bytes()
        digest = hashlib.sha256(data).hexdigest()
        if digest != expected: raise RuntimeError('upstream seal mismatch')
        if name.endswith('.node'):
            later = set(re.findall(rb'node_api_[A-Za-z0-9_]+', data)) - {b'node_api_module_get_api_version_v1'}
            if later: raise RuntimeError(f'API above N-API8: {slot} {later}')
        if data[:4] == b'\x7fELF':
            with original.open('rb') as stream:
                elf = ELFFile(stream)
                versions = elf.get_section_by_name('.gnu.version_r')
                if versions:
                    for _, auxiliaries in versions.iter_versions():
                        for aux in auxiliaries:
                            if aux.name.startswith('GLIBC_') and not (aux.entry['vna_flags'] & 2):
                                found = aux.name[6:]
                                if glibc is None or tuple(map(int,found.split('.'))) > tuple(map(int,glibc.split('.'))): glibc=found
        target = dest/slot/name
        target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copyfile(original,target)
        files[name]=digest
    if not files: raise RuntimeError('empty slot')
    manifest['slots'][slot]={'platform':platform,'arch':arch,'libc':libc,'glibc':glibc,'napi':8,'files':files}
    print(f'{slot}: {len(files)} unchanged native files; glibc={glibc}')
dest.mkdir(parents=True,exist_ok=True)
(dest/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')

for slot in ['win32-x64', 'win32-arm64']:
    relative = 'windows-process-tree.node'
    original = source/'targets'/slot/relative
    if hashlib.sha256(original.read_bytes()).hexdigest() != sealed['targets'][slot]['files'][relative]:
        raise RuntimeError('process-tree seal mismatch')
    target = root/'.build/windows-process-tree'/slot.split('-')[1]/relative
    target.parent.mkdir(parents=True,exist_ok=True)
    shutil.copyfile(original,target)
