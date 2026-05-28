import React from 'react';
import { cn } from '@/lib/utils';

interface ImperialSealProps extends React.SVGProps<SVGSVGElement> {
  size?: number;
  glow?: boolean;
}

/**
 * The 'Imperial Seal' (准) - Modern Laboratory Edition
 * Signifies verification, approval, and ground truth in the NDES/TokenScope ecosystem.
 */
export const ImperialSeal: React.FC<ImperialSealProps> = ({ 
  size = 48, 
  glow = true,
  className,
  ...props 
}) => {
  return (
    <svg 
      width={size} 
      height={size} 
      viewBox="0 0 100 100" 
      fill="none" 
      xmlns="http://www.w3.org/2000/svg"
      className={cn(
        "transition-all duration-700",
        glow && "drop-shadow-[0_0_15px_rgba(239,68,68,0.4)]",
        className
      )}
      {...props}
    >
      {/* Outer Border - Modern Box */}
      <rect x="10" y="10" width="80" height="80" stroke="currentColor" strokeWidth="2" className="text-red-500/30" />
      <rect x="15" y="15" width="70" height="70" stroke="currentColor" strokeWidth="1" className="text-red-500/50" />
      
      {/* Corner Brackets */}
      <path d="M10 25V10H25" stroke="currentColor" strokeWidth="3" className="text-red-500" />
      <path d="M75 10H90V25" stroke="currentColor" strokeWidth="3" className="text-red-500" />
      <path d="M90 75V90H75" stroke="currentColor" strokeWidth="3" className="text-red-500" />
      <path d="M25 90H10V75" stroke="currentColor" strokeWidth="3" className="text-red-500" />

      {/* The '准' Character - Geometric Technical Construction */}
      <g className="text-red-500 stroke-red-500">
        {/* Left Part (冫) */}
        <line x1="30" y1="35" x2="40" y2="45" strokeWidth="4" />
        <line x1="30" y1="65" x2="40" y2="55" strokeWidth="4" />
        
        {/* Right Part (隹) */}
        <path d="M50 30V75" strokeWidth="4" /> {/* Main Vertical */}
        <path d="M50 30C50 30 75 25 80 40" strokeWidth="3" fill="none" /> {/* Top Curve */}
        <line x1="50" y1="45" x2="75" y2="45" strokeWidth="3" />
        <line x1="50" y1="58" x2="75" y2="58" strokeWidth="3" />
        <line x1="50" y1="71" x2="75" y2="71" strokeWidth="3" />
      </g>
      
      {/* Technical Scanning Decoration */}
      <line x1="15" y1="50" x2="85" y2="50" stroke="currentColor" strokeWidth="0.5" className="text-red-500/20 animate-pulse" />
    </svg>
  );
};
