import React from 'react';
import { cn } from '@/lib/utils';

interface LabPanelProps extends React.HTMLAttributes<HTMLDivElement> {
  title?: string;
  status?: 'online' | 'busy' | 'offline';
  headerExtra?: React.ReactNode;
}

export const LabPanel: React.FC<LabPanelProps> = ({ 
  children, 
  title, 
  status, 
  headerExtra,
  className,
  ...props 
}) => {
  return (
    <div 
      className={cn(
        "lab-glass rounded-xl overflow-hidden flex flex-col group transition-all duration-300 hover:border-white/10",
        className
      )}
      {...props}
    >
      {(title || status || headerExtra) && (
        <div className="px-4 py-2 bg-white/2 border-b border-white/5 flex items-center justify-between">
          <div className="flex items-center gap-3">
            {status && (
              <div className={cn(
                "w-1.5 h-1.5 rounded-full animate-pulse shadow-[0_0_8px]",
                status === 'online' ? "bg-emerald-500 shadow-emerald-500/50" :
                status === 'busy' ? "bg-amber-500 shadow-amber-500/50" :
                "bg-slate-500 shadow-slate-500/50"
              )} />
            )}
            {title && (
              <h3 className="text-[10px] font-bold tracking-[0.2em] uppercase text-muted-foreground group-hover:text-foreground transition-colors overflow-hidden whitespace-nowrap overflow-ellipsis">
                {title}
              </h3>
            )}
          </div>
          {headerExtra && <div className="flex items-center gap-2">{headerExtra}</div>}
        </div>
      )}
      <div className="flex-1 p-4">
        {children}
      </div>
    </div>
  );
};
