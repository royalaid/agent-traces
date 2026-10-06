"""Package one agent-traces target exactly as .github/workflows/release.yml does."""
import hashlib, os, pathlib, shutil, sys

target, suffix = sys.argv[1], (sys.argv[2] if len(sys.argv) > 2 else '')
os.chdir(sys.argv[3] if len(sys.argv) > 3 else '.')
name = 'agent-traces-' + target
stage = pathlib.Path('dist') / name
if stage.exists():
    shutil.rmtree(stage)
stage.mkdir(parents=True)
shutil.copy2(pathlib.Path('target') / target / 'release' / ('agent-traces' + suffix), stage)
shutil.copy2('README.md', stage)
shutil.copytree('schemas', stage / 'schemas')
shutil.copytree('docs', stage / 'docs')
archive = pathlib.Path(shutil.make_archive(str(pathlib.Path('dist') / name), 'zip' if suffix else 'gztar', 'dist', name))
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_name(archive.name + '.sha256').write_text(digest + '  ' + archive.name + '\n', encoding='utf-8', newline='\n')
print(archive, digest)
