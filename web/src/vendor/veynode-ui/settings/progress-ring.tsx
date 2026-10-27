export function ProgressRing({completed, total, label}: {completed:number; total:number; label:string}) {
 const progress=total>0?Math.min(1,Math.max(0,completed/total)):0;
 return <svg role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={Math.max(1,total)} aria-valuenow={completed} viewBox="0 0 20 20" className="size-4 shrink-0 -rotate-90">
  <circle cx="10" cy="10" r="7" fill="none" stroke="currentColor" strokeWidth="3" className="text-line"/>
  <circle cx="10" cy="10" r="7" fill="none" stroke="currentColor" strokeWidth="3" pathLength="100" strokeDasharray={`${progress*100} 100`} strokeLinecap="round" className="text-ink transition-[stroke-dasharray] duration-[var(--motion-slow)] ease-standard motion-reduce:transition-none"/>
 </svg>;
}
