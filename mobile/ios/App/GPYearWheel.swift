import SwiftUI

/// Months run clockwise, starting with January at the top.
struct GPYearWheel: View {
    let counts: [Int]
    @Binding var selected: Int?
    @Environment(\.dynamicTypeSize) private var typeSize
    private let labels = ["янв", "фев", "мар", "апр", "май", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"]
    private func count(_ month: Int) -> Int { counts.indices.contains(month - 1) ? counts[month - 1] : 0 }
    var body: some View {
        if typeSize.isAccessibilitySize {
            Picker("Период", selection: $selected) {
                Text("Весь год · \(counts.reduce(0, +))").tag(nil as Int?)
                ForEach(1...12, id: \.self) { month in
                    Text("\(GPDate.months[month - 1]) · \(count(month))").tag(Optional(month))
                }
            }
            .pickerStyle(.menu)
        } else {
            wheel
        }
    }

    private var wheel: some View {
        GeometryReader { frame in
            let side = min(frame.size.width, frame.size.height)
            let cx = frame.size.width / 2
            let cy = frame.size.height / 2
            ZStack {
                Circle().stroke(Color.secondary.opacity(0.16), lineWidth: 5).frame(width: side * 0.59, height: side * 0.59)
                    .position(x: cx, y: cy).accessibilityHidden(true)
                ForEach(1...12, id: \.self) { month in
                    let angle = Double(month - 1) * .pi / 6 - .pi / 2
                    let active = selected == month
                    let total = count(month)
                    Circle().trim(from: CGFloat(month - 1) / 12 + 0.005, to: CGFloat(month) / 12 - 0.005)
                        .stroke(active ? Color.accentColor : Color.accentColor.opacity(total == 0 ? 0 : 0.55), lineWidth: active ? 7 : 5)
                        .rotationEffect(.degrees(-105)).frame(width: side * 0.59, height: side * 0.59).position(x: cx, y: cy)
                        .accessibilityHidden(true)
                    Button { selected = active ? nil : month } label: {
                        VStack(spacing: 0) {
                            Text(labels[month - 1]).font(.caption.weight(active ? .bold : .regular))
                            Text(String(total)).font(.caption2.monospacedDigit()).foregroundStyle(.secondary)
                        }
                        .frame(minWidth: 44, minHeight: 44).contentShape(Rectangle())
                    }
                    .buttonStyle(.plain).foregroundStyle(active ? Color.accentColor : Color.primary)
                    .position(x: cx + side * 0.407 * CGFloat(cos(angle)), y: cy + side * 0.407 * CGFloat(sin(angle)))
                    .accessibilityLabel(GPDate.months[month - 1]).accessibilityValue("Событий: \(total)")
                    .accessibilityAddTraits(active ? .isSelected : [])
                }
                Button { selected = nil } label: {
                    VStack(spacing: 2) {
                        Text(selected.flatMap { GPDate.months.indices.contains($0 - 1) ? GPDate.months[$0 - 1] : nil } ?? "Весь год").font(.subheadline.weight(.medium))
                        Text(String(selected.map(count) ?? counts.reduce(0, +))).font(.title.bold().monospacedDigit())
                    }
                    .frame(minWidth: 54, minHeight: 54)
                }.buttonStyle(.plain).position(x: cx, y: cy).accessibilityHint("Показать события за весь год")
            }
        }
        .frame(height: 252)
    }
}
