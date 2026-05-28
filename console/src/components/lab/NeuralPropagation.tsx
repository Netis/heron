import React from 'react';
import { cn } from '@/lib/utils';

export const NeuralPropagation: React.FC<{ className?: string }> = ({ className }) => {
  return (
    <div className={cn("relative w-full h-full min-h-[200px] flex items-center justify-center overflow-hidden bg-black/40 rounded-lg lab-scanline", className)}>
      <svg width="100%" height="100%" viewBox="0 0 800 300" className="opacity-80">
        <defs>
          <linearGradient id="lineGrad" x1="0%" y1="0%" x2="100%" y2="0%">
            <stop offset="0%" stopColor="#0ea5e9" stopOpacity="0.2" />
            <stop offset="50%" stopColor="#10b981" stopOpacity="0.8" />
            <stop offset="100%" stopColor="#0ea5e9" stopOpacity="0.2" />
          </linearGradient>
          <filter id="glow">
            <feGaussianBlur stdDeviation="2" result="coloredBlur"/>
            <feMerge>
              <feMergeNode in="coloredBlur"/>
              <feMergeNode in="SourceGraphic"/>
            </feMerge>
          </filter>
        </defs>

        {/* Neural Nodes */}
        {[...Array(6)].map((_, i) => (
          <g key={`col-${i}`}>
            {[...Array(4)].map((_, j) => (
              <circle 
                key={`node-${i}-${j}`}
                cx={100 + i * 120} 
                cy={60 + j * 60} 
                r="3" 
                className="fill-cyan-500/50"
              />
            ))}
          </g>
        ))}

        {/* Connection Lines with animation */}
        {[...Array(5)].map((_, i) => (
          <g key={`path-${i}`}>
            {[...Array(4)].map((_, j) => (
              <path 
                key={`line-${i}-${j}`}
                d={`M ${100 + i * 120} ${60 + j * 60} L ${100 + (i+1) * 120} ${60 + ((j+1)%4) * 60}`}
                stroke="url(#lineGrad)"
                strokeWidth="1"
                fill="none"
                strokeDasharray="20, 100"
                className="animate-[dash_3s_linear_infinite]"
                style={{ animationDelay: `${(i + j) * 0.2}s` }}
              />
            ))}
          </g>
        ))}

        {/* Pulsing Highlight Nodes */}
        <circle cx={340} cy={120} r="6" className="fill-emerald-500 animate-pulse" filter="url(#glow)" />
        <circle cx={580} cy={180} r="6" className="fill-cyan-500 animate-pulse" filter="url(#glow)" />
      </svg>
      
      {/* HUD overlay text */}
      <div className="absolute top-2 left-3 text-[8px] font-mono text-emerald-500/50 tracking-tighter">
        SIGNAL_PROP_L34_H12 :: REALTIME_STREAM
      </div>
      <div className="absolute bottom-2 right-3 text-[8px] font-mono text-cyan-500/50 tracking-tighter">
        DENSITY_MAPPING_NULL_SPACE
      </div>
      
      <style>{`
        @keyframes dash {
          to { stroke-dashoffset: -120; }
        }
      `}</style>
    </div>
  );
};
