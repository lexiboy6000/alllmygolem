//! A self-contained neovim-bindings demo page for end-to-end testing of the
//! "complete task" workflow. It is driven ENTIRELY by keystrokes (keydown):
//! - NORMAL mode: `i`/`a`/`o` enter insert, `:` opens the command line.
//! - INSERT mode: printable keys insert, Enter/Backspace edit, Esc → normal.
//! - COMMAND mode: type a command; Enter runs it (`:w`/`:wq`/`:x` "save").
//!
//! Crucially there is NO settable value (the content lives in a JS variable and
//! `window.__nvim`, not a form field) and paste is blocked — so the only way to
//! fill it is real keystrokes, defeating the "easy JS solution".

pub const NVIM_DEMO_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>nvim demo</title>
<style>
  body { background:#1e1e1e; color:#ddd; font-family:monospace; margin:0; }
  #bar { background:#333; padding:4px 10px; display:flex; gap:18px; }
  #editor { white-space:pre; padding:10px; outline:none; min-height:70vh; font-size:15px; line-height:1.35; }
  #editor:focus { box-shadow: inset 0 0 0 2px #2a6; }
  .cur { background:#8b8; color:#000; }
  #status { color:#8c8; } #cmd { color:#cc6; } #saves { color:#9ad; }
</style></head>
<body>
  <div id="bar"><span id="status">-- NORMAL --</span><span id="cmd"></span><span id="saves">saves: 0</span></div>
  <div id="editor" tabindex="0"></div>
<script>
(function(){
  var mode='normal', lines=[''], row=0, col=0, cmd='', saves=0;
  var count=0, countActive=false, pendingReplace=false;
  var editor=document.getElementById('editor');
  var statusEl=document.getElementById('status');
  var cmdEl=document.getElementById('cmd');
  var savesEl=document.getElementById('saves');

  function esc(s){ return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }
  function render(){
    var html='';
    for(var r=0;r<lines.length;r++){
      var L=lines[r];
      if(r===row){
        var c=Math.min(col, L.length);
        html += esc(L.slice(0,c)) + '<span class="cur">' + esc(L.slice(c,c+1)||' ') + '</span>' + esc(L.slice(c+1));
      } else { html += esc(L) || ' '; }
      html += '\n';
    }
    editor.innerHTML = html;
    statusEl.textContent = '-- ' + mode.toUpperCase() + ' --';
    cmdEl.textContent = (mode==='command') ? cmd : '';
    savesEl.textContent = 'saves: ' + saves;
    window.__nvim = { content: lines.join('\n'), saves: saves, mode: mode, lines: lines.length };
  }
  function insertChar(ch){ var L=lines[row]; lines[row]=L.slice(0,col)+ch+L.slice(col); col++; }
  function newline(){ var L=lines[row]; var rest=L.slice(col); lines[row]=L.slice(0,col); lines.splice(row+1,0,rest); row++; col=0; }
  function backspace(){
    if(col>0){ var L=lines[row]; lines[row]=L.slice(0,col-1)+L.slice(col); col--; }
    else if(row>0){ col=lines[row-1].length; lines[row-1]+=lines[row]; lines.splice(row,1); row--; }
  }
  function flash(){ document.body.style.background='#143'; setTimeout(function(){document.body.style.background='';},150); }

  function resetCount(){ count=0; countActive=false; }
  // This MUST mirror src/workflows/complete/nvim.rs (the self-test oracle).
  editor.addEventListener('keydown', function(e){
    var k=e.key;
    if(mode==='insert'){
      if(k==='Escape'){ mode='normal'; if(col>0)col--; }
      else if(k==='Enter'){ newline(); }
      else if(k==='Backspace'){ backspace(); }
      else if(k.length===1){ insertChar(k); }
      else { return; }
    } else if(mode==='normal'){
      var L=lines[row];
      if(pendingReplace){
        if(k.length===1 && col<L.length){ lines[row]=L.slice(0,col)+k+L.slice(col+1); }
        pendingReplace=false; resetCount();
      }
      else if(k===':'){ mode='command'; cmd=':'; resetCount(); }
      else if(k==='i'){ mode='insert'; }
      else if(k==='a'){ mode='insert'; if(col<L.length)col++; }
      else if(k==='r'){ pendingReplace=true; }
      else if(k==='k'){ var nk=countActive?Math.max(count,1):1; row=Math.max(0,row-nk); var mxk=Math.max(0,lines[row].length-1); if(col>mxk)col=mxk; resetCount(); }
      else if(k==='j'){ var nj=countActive?Math.max(count,1):1; row=Math.min(lines.length-1,row+nj); var mxj=Math.max(0,lines[row].length-1); if(col>mxj)col=mxj; resetCount(); }
      else if(k==='G'){ row=lines.length-1; col=0; resetCount(); }
      else if(k==='$'){ col=Math.max(0, lines[row].length-1); resetCount(); }
      else if(k==='h'){ var nh=countActive?Math.max(count,1):1; col=Math.max(0,col-nh); resetCount(); }
      else if(k==='l'){ var nl=countActive?Math.max(count,1):1; var mx=Math.max(0,lines[row].length-1); col=Math.min(col+nl,mx); resetCount(); }
      else if(k==='0'){ if(countActive){ count=count*10; } else { col=0; } }
      else if(k>='1'&&k<='9'){ count=count*10+(k.charCodeAt(0)-48); countActive=true; }
      else { resetCount(); return; }
    } else { /* command */
      if(k==='Enter'){
        var body=cmd.slice(1);
        if(body==='w'||body==='wq'||body==='x'||body==='wa'){ saves++; flash(); }
        else if(/^[0-9]+$/.test(body)){ var n=parseInt(body,10); row=Math.min(Math.max(n-1,0), lines.length-1); col=0; }
        cmd=''; mode='normal';
      }
      else if(k==='Escape'){ cmd=''; mode='normal'; pendingReplace=false; resetCount(); }
      else if(k==='Backspace'){ cmd=cmd.slice(0,-1); if(cmd===''){ mode='normal'; } }
      else if(k.length===1){ cmd+=k; }
      else { return; }
    }
    e.preventDefault();
    render();
  });
  // Defeat the "easy JS solution": no settable field, and paste is blocked.
  editor.addEventListener('paste', function(e){ e.preventDefault(); });
  window.addEventListener('paste', function(e){ e.preventDefault(); });
  editor.addEventListener('blur', function(){ setTimeout(function(){editor.focus();}, 0); });
  editor.focus();
  render();
})();
</script></body></html>
"##;
