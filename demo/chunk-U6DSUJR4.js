import{a as W}from"./chunk-ZGTFU5EI.js";import{a as Ce,c as ye,d as we,e as Me,f as Se,g as Ie,h as U}from"./chunk-BIKITC2C.js";import{A as ve,B as xe,C as j,D as $,k as X,y as be}from"./chunk-J6BT6DNA.js";import{a as he}from"./chunk-PGBDE6IK.js";import{b as A,c as z,f as H,i as ge,j as fe,k as _e,n as L}from"./chunk-HELAWRYD.js";import"./chunk-WS5YOOLJ.js";import{i as ue,j as N}from"./chunk-S4QRVSES.js";import{$a as w,$b as de,Cb as m,Eb as d,Lb as Y,Ma as a,Mb as y,Nb as J,Ob as s,P as ie,Pb as f,Q as V,Qb as b,Rb as ee,S as re,T as oe,Tb as E,U as S,Ua as ce,Ub as P,Va as C,Vb as G,Z as _,_ as h,ab as B,ac as me,fa as ae,fc as F,ga as se,jc as pe,ka as g,lb as K,mb as p,mc as te,nb as u,pb as I,qa as le,qb as k,rb as v,sb as r,tb as o,ub as M,vb as D,wb as R,xb as T,yb as x}from"./chunk-3SOZGYPJ.js";var ke=(()=>{class i{static \u0275fac=function(n){return new(n||i)};static \u0275mod=B({type:i});static \u0275inj=V({imports:[X]})}return i})();function Ve(i,t){i&1&&T(0,"div",2)}var Be=new re("MAT_PROGRESS_BAR_DEFAULT_OPTIONS");var Pe=(()=>{class i{_elementRef=S(le);_ngZone=S(se);_changeDetectorRef=S(pe);_renderer=S(ce);_cleanupTransitionEnd;constructor(){let e=be(),n=S(Be,{optional:!0});this._isNoopAnimation=e==="di-disabled",e==="reduced-motion"&&this._elementRef.nativeElement.classList.add("mat-progress-bar-reduced-motion"),n&&(n.color&&(this.color=this._defaultColor=n.color),this.mode=n.mode||this.mode)}_isNoopAnimation;get color(){return this._color||this._defaultColor}set color(e){this._color=e}_color;_defaultColor="primary";get value(){return this._value}set value(e){this._value=Ee(e||0),this._changeDetectorRef.markForCheck()}_value=0;get bufferValue(){return this._bufferValue||0}set bufferValue(e){this._bufferValue=Ee(e||0),this._changeDetectorRef.markForCheck()}_bufferValue=0;animationEnd=new ae;get mode(){return this._mode}set mode(e){this._mode=e,this._changeDetectorRef.markForCheck()}_mode="determinate";ngAfterViewInit(){this._ngZone.runOutsideAngular(()=>{this._cleanupTransitionEnd=this._renderer.listen(this._elementRef.nativeElement,"transitionend",this._transitionendHandler)})}ngOnDestroy(){this._cleanupTransitionEnd?.()}_getPrimaryBarTransform(){return`scaleX(${this._isIndeterminate()?1:this.value/100})`}_getBufferBarFlexBasis(){return`${this.mode==="buffer"?this.bufferValue:100}%`}_isIndeterminate(){return this.mode==="indeterminate"||this.mode==="query"}_transitionendHandler=e=>{this.animationEnd.observers.length===0||!e.target||!e.target.classList.contains("mdc-linear-progress__primary-bar")||(this.mode==="determinate"||this.mode==="buffer")&&this._ngZone.run(()=>this.animationEnd.next({value:this.value}))};static \u0275fac=function(n){return new(n||i)};static \u0275cmp=w({type:i,selectors:[["mat-progress-bar"]],hostAttrs:["role","progressbar","aria-valuemin","0","aria-valuemax","100","tabindex","-1",1,"mat-mdc-progress-bar","mdc-linear-progress"],hostVars:10,hostBindings:function(n,l){n&2&&(K("aria-valuenow",l._isIndeterminate()?null:l.value)("mode",l.mode),J("mat-"+l.color),y("_mat-animation-noopable",l._isNoopAnimation)("mdc-linear-progress--animation-ready",!l._isNoopAnimation)("mdc-linear-progress--indeterminate",l._isIndeterminate()))},inputs:{color:"color",value:[2,"value","value",te],bufferValue:[2,"bufferValue","bufferValue",te],mode:"mode"},outputs:{animationEnd:"animationEnd"},exportAs:["matProgressBar"],decls:7,vars:5,consts:[["aria-hidden","true",1,"mdc-linear-progress__buffer"],[1,"mdc-linear-progress__buffer-bar"],[1,"mdc-linear-progress__buffer-dots"],["aria-hidden","true",1,"mdc-linear-progress__bar","mdc-linear-progress__primary-bar"],[1,"mdc-linear-progress__bar-inner"],["aria-hidden","true",1,"mdc-linear-progress__bar","mdc-linear-progress__secondary-bar"]],template:function(n,l){n&1&&(D(0,"div",0),T(1,"div",1),p(2,Ve,1,0,"div",2),R(),D(3,"div",3),T(4,"span",4),R(),D(5,"div",5),T(6,"span",4),R()),n&2&&(a(),Y("flex-basis",l._getBufferBarFlexBasis()),a(),u(l.mode==="buffer"?2:-1),a(),Y("transform",l._getPrimaryBarTransform()))},styles:[`.mat-mdc-progress-bar {
  --mat-progress-bar-animation-multiplier: 1;
  display: block;
  text-align: start;
}
.mat-mdc-progress-bar[mode=query] {
  transform: scaleX(-1);
}
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__buffer-dots,
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__primary-bar,
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__secondary-bar,
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__bar-inner.mdc-linear-progress__bar-inner {
  animation: none;
}
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__primary-bar,
.mat-mdc-progress-bar._mat-animation-noopable .mdc-linear-progress__buffer-bar {
  transition: transform 1ms;
}

.mat-progress-bar-reduced-motion {
  --mat-progress-bar-animation-multiplier: 2;
}

.mdc-linear-progress {
  position: relative;
  width: 100%;
  transform: translateZ(0);
  outline: 1px solid transparent;
  overflow-x: hidden;
  transition: opacity 250ms 0ms cubic-bezier(0.4, 0, 0.6, 1);
  height: max(var(--mat-progress-bar-track-height, 4px), var(--mat-progress-bar-active-indicator-height, 4px));
}
@media (forced-colors: active) {
  .mdc-linear-progress {
    outline-color: CanvasText;
  }
}

.mdc-linear-progress__bar {
  position: absolute;
  top: 0;
  bottom: 0;
  margin: auto 0;
  width: 100%;
  animation: none;
  transform-origin: top left;
  transition: transform 250ms 0ms cubic-bezier(0.4, 0, 0.6, 1);
  height: var(--mat-progress-bar-active-indicator-height, 4px);
}
.mdc-linear-progress--indeterminate .mdc-linear-progress__bar {
  transition: none;
}
[dir=rtl] .mdc-linear-progress__bar {
  right: 0;
  transform-origin: center right;
}

.mdc-linear-progress__bar-inner {
  display: inline-block;
  position: absolute;
  width: 100%;
  animation: none;
  border-top-style: solid;
  border-color: var(--mat-progress-bar-active-indicator-color, var(--mat-sys-primary));
  border-top-width: var(--mat-progress-bar-active-indicator-height, 4px);
}

.mdc-linear-progress__buffer {
  display: flex;
  position: absolute;
  top: 0;
  bottom: 0;
  margin: auto 0;
  width: 100%;
  overflow: hidden;
  height: var(--mat-progress-bar-track-height, 4px);
  border-radius: var(--mat-progress-bar-track-shape, var(--mat-sys-corner-none));
}

.mdc-linear-progress__buffer-dots {
  background-image: radial-gradient(circle, var(--mat-progress-bar-track-color, var(--mat-sys-surface-variant)) calc(var(--mat-progress-bar-track-height, 4px) / 2), transparent 0);
  background-repeat: repeat-x;
  background-size: calc(calc(var(--mat-progress-bar-track-height, 4px) / 2) * 5);
  background-position: left;
  flex: auto;
  transform: rotate(180deg);
  animation: mdc-linear-progress-buffering calc(250ms * var(--mat-progress-bar-animation-multiplier)) infinite linear;
}
@media (forced-colors: active) {
  .mdc-linear-progress__buffer-dots {
    background-color: ButtonBorder;
  }
}
[dir=rtl] .mdc-linear-progress__buffer-dots {
  animation: mdc-linear-progress-buffering-reverse calc(250ms * var(--mat-progress-bar-animation-multiplier)) infinite linear;
  transform: rotate(0);
}

.mdc-linear-progress__buffer-bar {
  flex: 0 1 100%;
  transition: flex-basis 250ms 0ms cubic-bezier(0.4, 0, 0.6, 1);
  background-color: var(--mat-progress-bar-track-color, var(--mat-sys-surface-variant));
}

.mdc-linear-progress__primary-bar {
  transform: scaleX(0);
}
.mdc-linear-progress--indeterminate .mdc-linear-progress__primary-bar {
  left: -145.166611%;
}
.mdc-linear-progress--indeterminate.mdc-linear-progress--animation-ready .mdc-linear-progress__primary-bar {
  animation: mdc-linear-progress-primary-indeterminate-translate calc(2s * var(--mat-progress-bar-animation-multiplier)) infinite linear;
}
.mdc-linear-progress--indeterminate.mdc-linear-progress--animation-ready .mdc-linear-progress__primary-bar > .mdc-linear-progress__bar-inner {
  animation: mdc-linear-progress-primary-indeterminate-scale calc(2s * var(--mat-progress-bar-animation-multiplier)) infinite linear;
}
[dir=rtl] .mdc-linear-progress.mdc-linear-progress--animation-ready .mdc-linear-progress__primary-bar {
  animation-name: mdc-linear-progress-primary-indeterminate-translate-reverse;
}
[dir=rtl] .mdc-linear-progress.mdc-linear-progress--indeterminate .mdc-linear-progress__primary-bar {
  right: -145.166611%;
  left: auto;
}

.mdc-linear-progress__secondary-bar {
  display: none;
}
.mdc-linear-progress--indeterminate .mdc-linear-progress__secondary-bar {
  left: -54.888891%;
  display: block;
}
.mdc-linear-progress--indeterminate.mdc-linear-progress--animation-ready .mdc-linear-progress__secondary-bar {
  animation: mdc-linear-progress-secondary-indeterminate-translate calc(2s * var(--mat-progress-bar-animation-multiplier)) infinite linear;
}
.mdc-linear-progress--indeterminate.mdc-linear-progress--animation-ready .mdc-linear-progress__secondary-bar > .mdc-linear-progress__bar-inner {
  animation: mdc-linear-progress-secondary-indeterminate-scale calc(2s * var(--mat-progress-bar-animation-multiplier)) infinite linear;
}
[dir=rtl] .mdc-linear-progress.mdc-linear-progress--animation-ready .mdc-linear-progress__secondary-bar {
  animation-name: mdc-linear-progress-secondary-indeterminate-translate-reverse;
}
[dir=rtl] .mdc-linear-progress.mdc-linear-progress--indeterminate .mdc-linear-progress__secondary-bar {
  right: -54.888891%;
  left: auto;
}

@keyframes mdc-linear-progress-buffering {
  from {
    transform: rotate(180deg) translateX(calc(var(--mat-progress-bar-track-height, 4px) * -2.5));
  }
}
@keyframes mdc-linear-progress-primary-indeterminate-translate {
  0% {
    transform: translateX(0);
  }
  20% {
    animation-timing-function: cubic-bezier(0.5, 0, 0.701732, 0.495819);
    transform: translateX(0);
  }
  59.15% {
    animation-timing-function: cubic-bezier(0.302435, 0.381352, 0.55, 0.956352);
    transform: translateX(83.67142%);
  }
  100% {
    transform: translateX(200.611057%);
  }
}
@keyframes mdc-linear-progress-primary-indeterminate-scale {
  0% {
    transform: scaleX(0.08);
  }
  36.65% {
    animation-timing-function: cubic-bezier(0.334731, 0.12482, 0.785844, 1);
    transform: scaleX(0.08);
  }
  69.15% {
    animation-timing-function: cubic-bezier(0.06, 0.11, 0.6, 1);
    transform: scaleX(0.661479);
  }
  100% {
    transform: scaleX(0.08);
  }
}
@keyframes mdc-linear-progress-secondary-indeterminate-translate {
  0% {
    animation-timing-function: cubic-bezier(0.15, 0, 0.515058, 0.409685);
    transform: translateX(0);
  }
  25% {
    animation-timing-function: cubic-bezier(0.31033, 0.284058, 0.8, 0.733712);
    transform: translateX(37.651913%);
  }
  48.35% {
    animation-timing-function: cubic-bezier(0.4, 0.627035, 0.6, 0.902026);
    transform: translateX(84.386165%);
  }
  100% {
    transform: translateX(160.277782%);
  }
}
@keyframes mdc-linear-progress-secondary-indeterminate-scale {
  0% {
    animation-timing-function: cubic-bezier(0.205028, 0.057051, 0.57661, 0.453971);
    transform: scaleX(0.08);
  }
  19.15% {
    animation-timing-function: cubic-bezier(0.152313, 0.196432, 0.648374, 1.004315);
    transform: scaleX(0.457104);
  }
  44.15% {
    animation-timing-function: cubic-bezier(0.257759, -0.003163, 0.211762, 1.38179);
    transform: scaleX(0.72796);
  }
  100% {
    transform: scaleX(0.08);
  }
}
@keyframes mdc-linear-progress-primary-indeterminate-translate-reverse {
  0% {
    transform: translateX(0);
  }
  20% {
    animation-timing-function: cubic-bezier(0.5, 0, 0.701732, 0.495819);
    transform: translateX(0);
  }
  59.15% {
    animation-timing-function: cubic-bezier(0.302435, 0.381352, 0.55, 0.956352);
    transform: translateX(-83.67142%);
  }
  100% {
    transform: translateX(-200.611057%);
  }
}
@keyframes mdc-linear-progress-secondary-indeterminate-translate-reverse {
  0% {
    animation-timing-function: cubic-bezier(0.15, 0, 0.515058, 0.409685);
    transform: translateX(0);
  }
  25% {
    animation-timing-function: cubic-bezier(0.31033, 0.284058, 0.8, 0.733712);
    transform: translateX(-37.651913%);
  }
  48.35% {
    animation-timing-function: cubic-bezier(0.4, 0.627035, 0.6, 0.902026);
    transform: translateX(-84.386165%);
  }
  100% {
    transform: translateX(-160.277782%);
  }
}
@keyframes mdc-linear-progress-buffering-reverse {
  from {
    transform: translateX(-10px);
  }
}
`],encapsulation:2,changeDetection:0})}return i})();function Ee(i,t=0,e=100){return Math.max(t,Math.min(e,i))}var Ge=(()=>{class i{static \u0275fac=function(n){return new(n||i)};static \u0275mod=B({type:i});static \u0275inj=V({imports:[X]})}return i})();var O=class i{constructor(t){this.api=t}api;list(t={}){let e={};return t.subscribed!==void 0&&(e.subscribed=String(t.subscribed)),t.search&&(e.search=t.search),t.limit&&(e.limit=String(t.limit)),t.offset&&(e.offset=String(t.offset)),this.api.get("/groups",e)}refresh(){return this.api.post("/groups/refresh")}getStatus(t){return this.api.get(`/groups/${t}/status`)}subscribe(t){return this.api.post(`/groups/${t}/subscribe`)}unsubscribe(t){return this.api.post(`/groups/${t}/unsubscribe`)}listHeaders(t,e={}){let n={};return e.search&&(n.search=e.search),e.limit&&(n.limit=String(e.limit)),e.offset&&(n.offset=String(e.offset)),this.api.get(`/groups/${t}/headers`,n)}fetchHeaders(t){return this.api.post(`/groups/${t}/headers/fetch`)}markAllRead(t){return this.api.post(`/groups/${t}/headers/mark-all-read`)}downloadSelected(t,e,n,l){return this.api.post(`/groups/${t}/headers/download`,{message_ids:e,name:n,category:l})}getArticle(t){return this.api.get(`/articles/${encodeURIComponent(t)}`)}static \u0275fac=function(e){return new(e||i)(oe(he))};static \u0275prov=ie({token:i,factory:i.\u0275fac,providedIn:"root"})};var ze=(i,t)=>t.id;function He(i,t){i&1&&M(0,"mat-progress-bar",5)}function Le(i,t){if(i&1){let e=x();r(0,"div",7)(1,"span",12),s(2),o(),r(3,"span",13),s(4),de(5,"number"),o(),r(6,"button",14),m("click",function(){let l=_(e).$implicit,c=d();return h(c.toggleSub(l))}),s(7),o()()}if(i&2){let e=t.$implicit,n=d();a(2),f(e.name),a(2),f(me(5,6,e.article_count)),a(2),y("subscribed",e.subscribed),v("disabled",n.subPendingIds().has(e.id)),a(),b(" ",e.subscribed?"\u2605":"\u2606"," ")}}function Xe(i,t){i&1&&(r(0,"div",8),s(1,"No groups found. Click Refresh to load from server."),o())}function je(i,t){if(i&1){let e=x();r(0,"div",9)(1,"button",15),m("click",function(){_(e);let l=d();return h(l.loadMoreGroups())}),s(2),o()()}if(i&2){let e=d();a(2),b("Load more (",e.total()-e.groups().length," remaining)")}}var q=class i{constructor(t,e,n){this.svc=t;this.snack=e;this.dialogRef=n}svc;snack;dialogRef;groups=g([]);total=g(0);search="";offset=0;refreshing=g(!1);subPendingIds=g(new Set);ngOnInit(){this.loadGroups()}loadGroups(){this.offset=0,this.svc.list({search:this.search||void 0,limit:100,offset:0}).subscribe(t=>{this.groups.set(t.groups),this.total.set(t.total)})}loadMoreGroups(){this.offset+=100,this.svc.list({search:this.search||void 0,limit:100,offset:this.offset}).subscribe(t=>{this.groups.set([...this.groups(),...t.groups])})}refresh(){this.refreshing.set(!0),this.svc.refresh().subscribe({next:t=>{this.refreshing.set(!1),this.snack.open(t.message,"Close",{duration:3e3}),this.loadGroups()},error:t=>{this.refreshing.set(!1);let e=t.status===400?t.error?.human_readable||"No servers configured \u2014 add one in Settings first.":"Refresh failed";this.snack.open(e,"Close",{duration:5e3})}})}toggleSub(t){let e=t.subscribed,n=e?this.svc.unsubscribe(t.id):this.svc.subscribe(t.id),l=new Set(this.subPendingIds());l.add(t.id),this.subPendingIds.set(l);let c=()=>{let Z=new Set(this.subPendingIds());Z.delete(t.id),this.subPendingIds.set(Z)};n.subscribe({next:()=>{t.subscribed=!e,this.groups.set([...this.groups()]),this.snack.open(e?`Unsubscribed from ${t.name}`:`Subscribed to ${t.name}`,"Close",{duration:2e3}),c()},error:()=>{this.snack.open(`Failed to ${e?"unsubscribe from":"subscribe to"} ${t.name}`,"Close",{duration:4e3}),c()}})}static \u0275fac=function(e){return new(e||i)(C(O),C(j),C(Ce))};static \u0275cmp=w({type:i,selectors:[["app-group-browser-dialog"]],decls:17,vars:6,consts:[["mat-dialog-title",""],[1,"toolbar"],[1,"tool-btn",3,"click","disabled"],["name","retry",3,"size"],["placeholder","Search groups...",1,"search-input",3,"ngModelChange","keyup.enter","ngModel"],["mode","indeterminate"],[1,"group-list"],[1,"group-row"],[1,"empty"],[1,"more"],["align","end"],["mat-button","","mat-dialog-close",""],[1,"gname"],[1,"gcount"],[1,"sub-btn",3,"click","disabled"],[1,"tool-btn",3,"click"]],template:function(e,n){e&1&&(r(0,"h2",0),s(1,"Browse Newsgroups"),o(),r(2,"mat-dialog-content")(3,"div",1)(4,"button",2),m("click",function(){return n.refresh()}),M(5,"app-icon",3),s(6," Refresh from Server "),o(),r(7,"input",4),G("ngModelChange",function(c){return P(n.search,c)||(n.search=c),c}),m("keyup.enter",function(){return n.loadGroups()}),o()(),p(8,He,1,0,"mat-progress-bar",5),r(9,"div",6),I(10,Le,8,8,"div",7,ze),p(12,Xe,2,0,"div",8),o(),p(13,je,3,1,"div",9),o(),r(14,"mat-dialog-actions",10)(15,"button",11),s(16,"Close"),o()()),e&2&&(a(4),v("disabled",n.refreshing()),a(),v("size",11),a(2),E("ngModel",n.search),a(),u(n.refreshing()?8:-1),a(2),k(n.groups()),a(2),u(n.groups().length===0&&!n.refreshing()?12:-1),a(),u(n.total()>n.groups().length?13:-1))},dependencies:[N,L,A,z,H,U,we,Me,Ie,Se,xe,ve,ke,Ge,Pe,$,W,ue],styles:[".toolbar[_ngcontent-%COMP%]{display:flex;gap:8px;margin-bottom:8px}.search-input[_ngcontent-%COMP%]{flex:1;padding:6px 10px;background:var(--panel2);border:1px solid var(--line);border-radius:4px;color:var(--text);font-size:13px;outline:none}.search-input[_ngcontent-%COMP%]:focus{border-color:var(--accent)}.tool-btn[_ngcontent-%COMP%]{padding:5px 12px;border-radius:4px;border:1px solid var(--line);background:var(--panel2);color:var(--text);cursor:pointer;font-size:12px;white-space:nowrap}.tool-btn[_ngcontent-%COMP%]:hover{border-color:var(--accent)}.tool-btn[_ngcontent-%COMP%]:disabled{opacity:.4}.group-list[_ngcontent-%COMP%]{max-height:400px;overflow-y:auto}.group-row[_ngcontent-%COMP%]{display:flex;align-items:center;padding:6px 4px;border-bottom:1px solid var(--panel2);font-size:13px}.gname[_ngcontent-%COMP%]{flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.gcount[_ngcontent-%COMP%]{width:80px;text-align:right;color:var(--mute);font-size:12px;padding:0 8px}.sub-btn[_ngcontent-%COMP%]{background:none;border:none;font-size:18px;cursor:pointer;color:var(--mute);padding:0 4px}.sub-btn.subscribed[_ngcontent-%COMP%]{color:var(--warn)}.sub-btn[_ngcontent-%COMP%]:disabled{opacity:.4;cursor:wait}.empty[_ngcontent-%COMP%]{padding:24px;text-align:center;color:var(--mute)}.more[_ngcontent-%COMP%]{text-align:center;padding:8px}"]})};var ne=(i,t)=>t.id;function $e(i,t){if(i&1&&(r(0,"option",7),s(1),o()),i&2){let e=t.$implicit;v("value",e.name),a(),f(e.name)}}function We(i,t){if(i&1&&(s(0," \xB7 "),r(1,"span",22),s(2),o()),i&2){let e=d().$implicit;a(2),b("",e.unread_count," new")}}function Ue(i,t){if(i&1){let e=x();r(0,"div",19),m("click",function(){let l=_(e).$implicit,c=d();return h(c.selectGroup(l))}),r(1,"div",20),s(2),o(),r(3,"div",21),s(4),p(5,We,3,1),o()()}if(i&2){let e,n=t.$implicit,l=d();y("active",((e=l.selectedGroup())==null?null:e.id)===n.id),a(2),f(n.name),a(2),b(" ",n.article_count||0," headers "),a(),u(n.unread_count>0?5:-1)}}function Qe(i,t){if(i&1){let e=x();r(0,"div",17)(1,"button",23),m("click",function(){_(e);let l=d();return h(l.openBrowseDialog())}),s(2,"+ Subscribe to groups"),o()()}}function qe(i,t){i&1&&(r(0,"div",0)(1,"div",24)(2,"div",25),s(3," Pick a group on the left to browse headers, or use the search bar at the top to query across all subscribed groups. "),o()()())}function Ze(i,t){if(i&1&&s(0),i&2){let e=d(2);b(' \xB7 "',e.searchQuery,'" ')}}function Ke(i,t){if(i&1&&(s(0," \xB7 "),r(1,"span",22),s(2),o()),i&2){let e=d(2);a(2),b("",e.newAvailable()," new to fetch")}}function Ye(i,t){i&1&&s(0," Fetching\u2026 ")}function Je(i,t){i&1&&(M(0,"app-icon",36),s(1," Fetch ")),i&2&&v("size",11)}function et(i,t){if(i&1){let e=x();r(0,"tr",37),m("click",function(){let l=_(e).$implicit,c=d(2);return h(c.selectArticle(l))}),r(1,"td",37),m("click",function(l){return l.stopPropagation()}),r(2,"input",31),m("change",function(){let l=_(e).$implicit,c=d(2);return h(c.toggleSelect(l.message_id))}),o()(),r(3,"td",38),s(4),o(),r(5,"td",39),s(6),o(),r(7,"td"),s(8),o(),r(9,"td",39),s(10),o(),r(11,"td",40)(12,"button",23),m("click",function(l){let c=_(e).$implicit;return d(2).selectArticle(c),h(l.stopPropagation())}),s(13," view "),o()()()}if(i&2){let e=t.$implicit,n=d(2);y("unread",!e.read),a(2),v("checked",n.isSelected(e.message_id)),a(2),f(e.subject),a(2),f(e.author),a(2),f(n.formatBytes(e.bytes)),a(2),f(e.date)}}function tt(i,t){if(i&1&&(s(0," Click "),r(1,"b"),s(2,"\u21BB Fetch"),o(),s(3)),i&2){let e=d(3);a(3),b(" to pull ",e.newAvailable()," new. ")}}function nt(i,t){if(i&1&&(r(0,"tr")(1,"td",41),s(2," No headers. "),p(3,tt,4,1),o()()),i&2){let e=d(2);a(3),u(e.newAvailable()>0?3:-1)}}function it(i,t){if(i&1){let e=x();r(0,"div",34)(1,"button",27),m("click",function(){_(e);let l=d(2);return h(l.loadMore())}),s(2,"Load more\u2026"),o()()}}function rt(i,t){if(i&1){let e=x();r(0,"div",35)(1,"span"),s(2),o(),M(3,"span",42),r(4,"button",8),m("click",function(){_(e);let l=d(2);return h(l.downloadSelected())}),s(5," \u2193 Download selected "),o()()}if(i&2){let e=d(2);a(2),ee("",e.selectedIds().length," selected \xB7 ",e.formatBytes(e.selectedBytes()))}}function ot(i,t){i&1&&(r(0,"div",44),s(1,"Loading article\u2026"),o())}function at(i,t){if(i&1&&(r(0,"pre",45),s(1),o()),i&2){let e=d(3);a(),f(e.articleBody()||"(empty)")}}function st(i,t){if(i&1&&(r(0,"div",0)(1,"h3"),s(2," Article preview "),r(3,"span",1)(4,"code"),s(5),o()()(),r(6,"div",2)(7,"div",43)(8,"div")(9,"b"),s(10,"From:"),o(),r(11,"span",39),s(12),o()(),r(13,"div")(14,"b"),s(15,"Subject:"),o(),s(16),o(),r(17,"div")(18,"b"),s(19,"Size:"),o(),r(20,"span",39),s(21),o()()(),p(22,ot,2,0,"div",44)(23,at,2,1,"pre",45),o()()),i&2){let e=t,n=d(2);a(5),f(e.message_id),a(7),f(e.author),a(4),b(" ",e.subject),a(5),f(n.formatBytes(e.bytes)),a(),u(n.articleLoading()?22:23)}}function lt(i,t){if(i&1){let e=x();r(0,"div",0)(1,"h3"),s(2," Results \u2014 "),r(3,"code"),s(4),o(),p(5,Ze,1,1),r(6,"span",1),s(7),p(8,Ke,3,1),o(),r(9,"button",26),m("click",function(){_(e);let l=d();return h(l.fetchHeaders())}),p(10,Ye,1,0)(11,Je,2,1),o(),r(12,"button",27),m("click",function(){_(e);let l=d();return h(l.markAllRead())}),s(13,"\u2713 Mark read"),o()(),r(14,"div",28)(15,"table",29)(16,"thead")(17,"tr")(18,"th",30)(19,"input",31),m("change",function(){_(e);let l=d();return h(l.toggleSelectAll())}),o()(),r(20,"th",32),s(21,"Subject"),o(),r(22,"th"),s(23,"Author"),o(),r(24,"th"),s(25,"Size"),o(),r(26,"th"),s(27,"Date"),o(),M(28,"th"),o()(),r(29,"tbody"),I(30,et,14,7,"tr",33,ne),p(32,nt,4,1,"tr"),o()()(),p(33,it,3,0,"div",34),p(34,rt,6,2,"div",35),o(),p(35,st,24,5,"div",0)}if(i&2){let e,n=d();a(4),f(n.selectedGroup().name),a(),u(n.searchQuery?5:-1),a(2),ee(" ",n.headerTotal()," match",n.headerTotal()===1?"":"es"," "),a(),u(n.newAvailable()>0?8:-1),a(),v("disabled",n.fetching()),a(),u(n.fetching()?10:11),a(9),v("checked",n.allSelected()),a(11),k(n.headers()),a(2),u(n.headers().length===0&&!n.fetching()?32:-1),a(),u(n.headerTotal()>n.headers().length?33:-1),a(),u(n.selectedIds().length>0?34:-1),a(),u((e=n.previewHeader())?35:-1,e)}}var Te=class i{constructor(t,e,n){this.svc=t;this.snack=e;this.dialog=n}svc;snack;dialog;groups=g([]);selectedGroup=g(null);groupFilter="";groupNameFilter="";headers=g([]);headerTotal=g(0);searchQuery="";offset=0;pageSize=100;selectedIds=g([]);newAvailable=g(0);fetching=g(!1);previewHeader=g(null);articleBody=g(null);articleLoading=g(!1);filteredGroups=F(()=>{let t=this.groupNameFilter.toLowerCase();return t?this.groups().filter(e=>e.name.toLowerCase().includes(t)):this.groups()});allSelected=F(()=>{let t=this.selectedIds(),e=this.headers();return e.length>0&&e.every(n=>t.includes(n.message_id))});selectedBytes=F(()=>{let t=new Set(this.selectedIds());return this.headers().filter(e=>t.has(e.message_id)).reduce((e,n)=>e+n.bytes,0)});ngOnInit(){this.loadGroups()}loadGroups(){this.svc.list({subscribed:!0,limit:500}).subscribe(t=>this.groups.set(t.groups))}selectGroup(t){this.selectedGroup.set(t),this.offset=0,this.searchQuery="",this.selectedIds.set([]),this.previewHeader.set(null),this.articleBody.set(null),this.loadHeaders(),this.loadStatus()}loadHeaders(){let t=this.selectedGroup();t&&this.svc.listHeaders(t.id,{search:this.searchQuery||void 0,limit:this.pageSize,offset:this.offset}).subscribe(e=>{this.headers.set(e.headers),this.headerTotal.set(e.total)})}loadStatus(){let t=this.selectedGroup();t&&this.svc.getStatus(t.id).subscribe(e=>this.newAvailable.set(e.new_available))}searchHeaders(){if(this.offset=0,this.groupFilter){let t=this.groups().find(e=>e.name===this.groupFilter);t&&t.id!==this.selectedGroup()?.id&&this.selectedGroup.set(t)}this.loadHeaders()}loadMore(){this.offset+=this.pageSize;let t=this.selectedGroup();t&&this.svc.listHeaders(t.id,{search:this.searchQuery||void 0,limit:this.pageSize,offset:this.offset}).subscribe(e=>this.headers.set([...this.headers(),...e.headers]))}fetchHeaders(){let t=this.selectedGroup();if(!t)return;this.fetching.set(!0),this.svc.fetchHeaders(t.id).subscribe({next:()=>this.snack.open("Fetching headers\u2026","Close",{duration:2e3}),error:()=>this.fetching.set(!1)});let e=setInterval(()=>{this.loadHeaders(),this.loadStatus(),this.loadGroups(),this.newAvailable()<=0&&(this.fetching.set(!1),clearInterval(e),clearTimeout(n))},3e3),n=setTimeout(()=>{clearInterval(e),this.fetching.set(!1),this.snack.open("Header fetch is taking longer than expected \u2014 it may still finish in the background. Refresh to check.","Close",{duration:6e3})},12e4)}markAllRead(){let t=this.selectedGroup();t&&this.svc.markAllRead(t.id).subscribe(()=>{this.loadHeaders(),this.loadGroups(),this.snack.open("All marked read","Close",{duration:2e3})})}toggleSelect(t){let e=this.selectedIds();this.selectedIds.set(e.includes(t)?e.filter(n=>n!==t):[...e,t])}isSelected(t){return this.selectedIds().includes(t)}toggleSelectAll(){this.selectedIds.set(this.allSelected()?[]:this.headers().map(t=>t.message_id))}selectArticle(t){this.previewHeader.set(t),this.articleLoading.set(!0),this.articleBody.set(null),this.svc.getArticle(t.message_id).subscribe({next:e=>{this.articleBody.set(e.body),this.articleLoading.set(!1),t.read||(t.read=!0,this.headers.set([...this.headers()]))},error:()=>{this.articleBody.set("(Failed to load)"),this.articleLoading.set(!1)}})}downloadSelected(){let t=this.selectedGroup();!t||!this.selectedIds().length||this.svc.downloadSelected(t.id,this.selectedIds()).subscribe({next:e=>{this.snack.open(e.message,"Close",{duration:3e3}),this.selectedIds.set([])},error:()=>this.snack.open("Download failed","Close",{duration:5e3})})}openBrowseDialog(){this.dialog.open(q,{width:"700px",maxHeight:"80vh"}).afterClosed().subscribe(()=>this.loadGroups())}formatBytes(t){if(t===0)return"0 B";let e=1024,n=["B","KB","MB","GB","TB"],l=Math.min(4,Math.floor(Math.log(t)/Math.log(e)));return(t/Math.pow(e,l)).toFixed(1)+" "+n[l]}static \u0275fac=function(e){return new(e||i)(C(O),C(j),C(ye))};static \u0275cmp=w({type:i,selectors:[["app-groups-view"]],decls:30,vars:6,consts:[[1,"panel"],[1,"hint"],[1,"body"],[1,"search-bar"],["placeholder","Search title, poster, subject\u2026",3,"ngModelChange","keyup.enter","ngModel"],[3,"ngModelChange","ngModel"],["value",""],[3,"value"],[1,"btn","primary",3,"click"],[1,"btn",3,"click"],[1,"shell"],[1,"side"],[1,"side-head"],[1,"side-filter"],["placeholder","Filter\u2026",3,"ngModelChange","ngModel"],[1,"side-list"],[1,"g",3,"active"],[1,"empty"],[1,"main"],[1,"g",3,"click"],[1,"name"],[1,"cnt"],[1,"new"],[1,"row-action",3,"click"],[1,"body","ctr"],[1,"big-hint"],[1,"btn","sm",3,"click","disabled"],[1,"btn","sm",3,"click"],[1,"body","flush"],[1,"data"],[2,"width","32px"],["type","checkbox",3,"change","checked"],[2,"width","48%"],[3,"unread"],[1,"body","load-more"],[1,"body","download-bar"],["name","retry",3,"size"],[3,"click"],[1,"subj"],[1,"dim"],[1,"actions"],["colspan","6",1,"empty-cell"],[1,"spacer"],[1,"meta"],[1,"loading"],[1,"body-pre"]],template:function(e,n){e&1&&(r(0,"div",0)(1,"h3"),s(2," Search Usenet headers "),r(3,"span",1),s(4,"SQLite FTS5 over XOVER-fetched headers"),o()(),r(5,"div",2)(6,"div",3)(7,"input",4),G("ngModelChange",function(c){return P(n.searchQuery,c)||(n.searchQuery=c),c}),m("keyup.enter",function(){return n.searchHeaders()}),o(),r(8,"select",5),G("ngModelChange",function(c){return P(n.groupFilter,c)||(n.groupFilter=c),c}),r(9,"option",6),s(10,"All subscribed groups"),o(),I(11,$e,2,2,"option",7,ne),o(),r(13,"button",8),m("click",function(){return n.searchHeaders()}),s(14,"Search"),o(),r(15,"button",9),m("click",function(){return n.openBrowseDialog()}),s(16,"+ Subscribe"),o()()()(),r(17,"div",10)(18,"aside",11)(19,"div",12),s(20),o(),r(21,"div",13)(22,"input",14),G("ngModelChange",function(c){return P(n.groupNameFilter,c)||(n.groupNameFilter=c),c}),o()(),r(23,"div",15),I(24,Ue,6,5,"div",16,ne),p(26,Qe,3,0,"div",17),o()(),r(27,"div",18),p(28,qe,4,0,"div",0)(29,lt,36,12),o()()),e&2&&(a(7),E("ngModel",n.searchQuery),a(),E("ngModel",n.groupFilter),a(3),k(n.groups()),a(9),b("Subscribed (",n.groups().length,")"),a(2),E("ngModel",n.groupNameFilter),a(2),k(n.filteredGroups()),a(2),u(n.groups().length===0?26:-1),a(2),u(n.selectedGroup()?29:28))},dependencies:[N,L,fe,_e,A,ge,z,H,$,U,W],styles:["[_nghost-%COMP%]{display:block}.shell[_ngcontent-%COMP%]{display:grid;grid-template-columns:260px 1fr;gap:16px;align-items:stretch}.side[_ngcontent-%COMP%]{background:var(--panel);border:1px solid var(--line);border-radius:8px;display:flex;flex-direction:column;align-self:stretch;height:100%;min-height:100%}.side-head[_ngcontent-%COMP%]{padding:10px 14px;border-bottom:1px solid var(--line);font-size:12px;color:var(--mute);text-transform:uppercase;letter-spacing:.5px}.side-filter[_ngcontent-%COMP%]{padding:8px 10px;border-bottom:1px solid var(--line)}.side-filter[_ngcontent-%COMP%]   input[_ngcontent-%COMP%]{width:100%;box-sizing:border-box;background:var(--panel2);border:1px solid var(--line);color:var(--text);padding:6px 10px;border-radius:5px;font:inherit;outline:none}.side-filter[_ngcontent-%COMP%]   input[_ngcontent-%COMP%]:focus{border-color:var(--accent)}.side-list[_ngcontent-%COMP%]{flex:1 1 auto;min-height:0;overflow-y:auto}.g[_ngcontent-%COMP%]{padding:8px 14px;border-bottom:1px solid var(--line);cursor:pointer}.g[_ngcontent-%COMP%]:last-child{border-bottom:none}.g[_ngcontent-%COMP%]:hover{background:var(--panel2)}.g.active[_ngcontent-%COMP%]{background:var(--panel2);box-shadow:inset 2px 0 0 var(--accent)}.g[_ngcontent-%COMP%]   .name[_ngcontent-%COMP%]{font-size:12px;font-family:ui-monospace,Menlo,monospace}.g[_ngcontent-%COMP%]   .cnt[_ngcontent-%COMP%]{color:var(--mute);font-size:11px;margin-top:2px}.g[_ngcontent-%COMP%]   .new[_ngcontent-%COMP%]{color:var(--accent2)}.empty[_ngcontent-%COMP%]{padding:16px 14px;color:var(--mute);font-size:12px;text-align:center}.main[_ngcontent-%COMP%]{min-width:0;display:flex;flex-direction:column;gap:16px;height:100%}.ctr[_ngcontent-%COMP%]{text-align:center;padding:48px 16px}.big-hint[_ngcontent-%COMP%]{color:var(--mute);font-size:14px}.new[_ngcontent-%COMP%]{color:var(--accent2)}tr.unread[_ngcontent-%COMP%]   .subj[_ngcontent-%COMP%]{font-weight:600}td.dim[_ngcontent-%COMP%]{color:var(--mute)}td.subj[_ngcontent-%COMP%]{max-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}tr[_ngcontent-%COMP%]{cursor:pointer}.actions[_ngcontent-%COMP%]{white-space:nowrap}.empty-cell[_ngcontent-%COMP%]{text-align:center;padding:28px!important;color:var(--mute);font-size:13px}.load-more[_ngcontent-%COMP%]{text-align:center;border-top:1px solid var(--line)}.download-bar[_ngcontent-%COMP%]{display:flex;align-items:center;gap:10px;border-top:1px solid var(--line);font-size:13px}.spacer[_ngcontent-%COMP%]{flex:1}.meta[_ngcontent-%COMP%]{display:flex;flex-direction:column;gap:4px;font-size:13px;margin-bottom:10px}.meta[_ngcontent-%COMP%]   b[_ngcontent-%COMP%]{color:var(--mute);font-weight:500;margin-right:6px}.body-pre[_ngcontent-%COMP%]{margin:0;background:var(--panel2);border:1px solid var(--line);border-radius:5px;padding:10px 12px;font:12px ui-monospace,Menlo,Consolas,monospace;max-height:300px;overflow:auto;white-space:pre-wrap;word-break:break-all}.loading[_ngcontent-%COMP%]{color:var(--mute);font-size:13px;padding:20px;text-align:center}"]})};export{Te as GroupsViewComponent};
