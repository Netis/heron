import React from 'react';
import { cn } from '@/lib/utils';

interface TokenDetail {
  id: string;
  text: string;
  prob: number;
  entropy?: number;
  isModel?: boolean;
}

interface TokenForensicProps {
  tokens: TokenDetail[];
  className?: string;
}

export const TokenForensic: React.FC<TokenForensicProps> = ({ tokens, className }) => {
  return (
    <div className={cn("flex flex-col gap-1 font-mono text-xs", className)}>
      {tokens.map((token, index) => (
        <div 
          key={token.id || index}
          className="group flex items-center gap-4 py-1.5 px-3 rounded hover:bg-white/5 border border-transparent hover:border-white/5 transition-all"
        >
          {/* Index / Meta */}
          <span className="w-8 text-[10px] text-muted-foreground/50">
            {String(index + 1).padStart(3, '0')}
          </span>

          {/* Token Text */}
          <span className={cn(
            "flex-1 px-2 py-0.5 rounded border border-white/10",
            token.isModel ? "bg-cyan-500/10 text-cyan-400 border-cyan-500/20" : "bg-emerald-500/10 text-emerald-400 border-emerald-500/20"
          )}>
            {token.text.replace(/\n/g, '↵').replace(/ /g, '·')}
          </span>

          {/* Probability Visualizer */}
          <div className="w-24 h-1.5 bg-white/5 rounded-full overflow-hidden relative">
            <div 
              className={cn(
                "h-full transition-all duration-500",
                token.prob > 0.8 ? "bg-emerald-500" :
                token.prob > 0.4 ? "bg-cyan-500" :
                "bg-red-500"
              )}
              style={{ width: `${token.prob * 100}%` }}
            />
          </div>

          {/* Percentage */}
          <span className="w-10 text-[10px] text-right text-muted-foreground">
            {(token.prob * 100).toFixed(1)}%
          </span>

          {/* Entropy (Optional) */}
          {token.entropy !== undefined && (
            <span className="text-[10px] text-muted-foreground/30 hidden lg:inline">
              H: {token.entropy.toFixed(2)}
            </span>
          )}
        </div>
      ))}
    </div>
  );
};
