import {connectPluginFrame} from './bridge.mjs';
const ui=await connectPluginFrame();const status=document.querySelector('#status'),config=document.querySelector('#config');
const context=await ui.call('context');document.documentElement.lang=context.locale;document.documentElement.dataset.theme=context.theme;
const batch=['converter','declick'].includes(context.plugin_id);let job=null;
status.textContent='Connecté';document.querySelector('h1').textContent=batch?'Traitement de fichiers':'Configuration du plugin';
const perform=async action=>{try{status.textContent=JSON.stringify(await action(),null,2);}catch(e){status.textContent=e.message;}};
if(batch){
 config.value=JSON.stringify(context.plugin_id==='converter'?{sources:[{track_id:1}],format:'flac'}:{sources:[{track_id:1}],options:{output_format:'flac',threshold_db:-60}},null,2);
 document.querySelector('#load').textContent='Capacités';document.querySelector('#save').textContent='Démarrer';
 document.querySelector('#load').onclick=()=>perform(()=>ui.call('capabilities'));
 document.querySelector('#save').onclick=()=>perform(async()=>{const result=await ui.call('start',JSON.parse(config.value));job=result.job_id;return result;});
 for(const [label,method] of [['État','status'],['Annuler','cancel'],['Lien de téléchargement','downloadUrl']]){const button=document.createElement('button');button.textContent=label;button.onclick=()=>perform(()=>{if(!job)throw Error('Démarrer un travail d’abord');return ui.call(method,job);});document.querySelector('div').append(button);}
}else{
 document.querySelector('#load').onclick=()=>perform(async()=>{const result=await ui.call('configuration');config.value=JSON.stringify(result,null,2);return result;});
 document.querySelector('#save').onclick=()=>perform(()=>ui.call('configure',JSON.parse(config.value)));
}
window.addEventListener('pagehide',()=>ui.close(),{once:true});
